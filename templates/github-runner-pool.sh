#!/bin/bash
# The github-runner-pool service (templates/github-runner-pool.json): one virtual machine that runs many fresh GitHub
# Actions runners. This one file is both halves of it.
#
# - Run with no argument it is the machine's first-boot script: cloud-init runs it once, as root. It installs Docker and
#   the few tools the controller uses from the distribution's own packages, copies itself to
#   /usr/local/sbin/arca-runner-pool and starts the controller as a systemd service.
# - `arca-runner-pool run` is the controller. It keeps a set of slots. Each slot registers one just-in-time runner with
#   GitHub (ephemeral: it takes one job and is then removed), runs it in a fresh container beside a Docker daemon of its
#   own, records what the job used, removes both and starts over with a new registration.
#
# What it reads:
# - /etc/arca/settings.json: the service's settings (the template's, with what its service_create replaced), written
#   by cloud-init at the first boot and fixed from then on.
# - /etc/arca-secrets/<NAME>: the template's secrets, written by Arca at each start (service_secret_set, then
#   instance_start). Either GITHUB_APP_CLIENT_ID and GITHUB_APP_PRIVATE_KEY (a GitHub App installed on the organisation
#   or repository), or GITHUB_TOKEN (a fine-grained token). When both are there the App is used. The controller mints
#   every short-lived token itself.
#
# What it writes: a status block and one line per finished job on the machine's console (service_logs), the same job
# lines in /var/log/arca-runner-pool/jobs.tsv (bounded: one older file is kept), and the last status block in
# /var/log/arca-runner-pool/status.txt.
#
# No credential is ever printed or put on a command line. A token reaches curl on its standard input. A runner's
# just-in-time configuration reaches its container as a file on /run (memory), which the container's own command
# reads into the runner's environment: it is not in Docker's record of the container on disk. The installation token
# and a key that had to be put back into lines are kept in /run/arca-runner-pool, which only root enters.
#
# Every path, pause and external program below can be replaced through the environment, which is how the test in
# crates/arca/tests/runner_pool.rs drives the controller against a fake GitHub and a fake docker.
set -u

: "${POOL_SETTINGS:=/etc/arca/settings.json}"
: "${POOL_SECRETS:=/etc/arca-secrets}"
: "${POOL_STATE:=/var/lib/arca-runner-pool}"
: "${POOL_LOGS:=/var/log/arca-runner-pool}"
: "${POOL_RUN:=/run/arca-runner-pool}"
: "${POOL_CGROUP:=/sys/fs/cgroup}"
: "${POOL_MEMINFO:=/proc/meminfo}"
: "${POOL_UNITS:=/run/systemd/system}"
: "${POOL_TICK:=5}"            # seconds between two passes of the controller
: "${POOL_POLL:=3}"            # seconds between two looks at a running slot
: "${POOL_STATUS_EVERY:=60}"   # seconds between two status blocks
: "${POOL_READY_WAIT:=60}"     # seconds a slot's Docker daemon is given to come up
: "${POOL_BACKOFF_MAX:=300}"   # the longest pause after failures, in seconds
: "${POOL_STOP_WAIT:=60}"      # seconds the slots are given to clean up when the controller stops
: "${POOL_TOKEN_TTL:=3000}"    # seconds an installation token is used; GitHub's lasts 3600
: "${POOL_REFUSE_EVERY:=300}"  # seconds between two tellings of why the pool does not start
: "${POOL_JOBS_MAX:=4194304}"  # bytes of jobs.tsv before it is set aside as jobs.tsv.1
: "${POOL_STATUS_JOBS:=10}"    # finished jobs repeated in each status block
: "${POOL_GROUP_EVERY:=600}"   # seconds between two reads of the runner group (or the repository) on GitHub

INSTALLED=/usr/local/sbin/arca-runner-pool
# The runner image's own user and docker group (the image fixes both).
RUNNER_UID=1001
DOCKER_GID=123

say() { printf 'arca-runner-pool: %s\n' "$*"; }
now() { date +%s; }
# A pause a signal can end at once.
nap() {
  sleep "$1" &
  wait $! 2>/dev/null
}

# ---------------------------------------------------------------------------------------------------------------------
# First boot
# ---------------------------------------------------------------------------------------------------------------------

install_pool() {
  export DEBIAN_FRONTEND=noninteractive
  local try
  for try in 1 2 3 4 5 6; do
    if apt-get update -q && apt-get install -y -q --no-install-recommends \
      docker.io curl jq openssl ca-certificates util-linux coreutils; then
      break
    fi
    say "first boot: the packages could not be installed (try $try of 6); trying again in 20 seconds"
    sleep 20
  done
  if ! command -v docker >/dev/null || ! command -v jq >/dev/null; then
    say "first boot failed: docker and jq are not installed. The machine has no pool; service_remove it and create it again once it can reach its package mirror"
    exit 1
  fi
  mkdir -p /etc/docker
  if [ ! -e /etc/docker/daemon.json ]; then
    # Slot containers are placed in systemd slices, so their memory is capped and measured per slot.
    printf '%s\n' '{ "exec-opts": ["native.cgroupdriver=systemd"], "log-driver": "local", "log-opts": { "max-size": "20m", "max-file": "2" } }' \
      >/etc/docker/daemon.json
  fi
  install -m 0755 "$0" "$INSTALLED"
  cat >/etc/systemd/system/arca-runner-pool.service <<UNIT
[Unit]
Description=Arca GitHub Actions runner pool
After=docker.service network-online.target
Wants=network-online.target
Requires=docker.service

[Service]
ExecStart=$INSTALLED run
Restart=always
RestartSec=10
KillMode=mixed
TimeoutStopSec=150
StandardOutput=journal+console
StandardError=journal+console

[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl restart docker
  systemctl enable --now arca-runner-pool.service
  say "first boot done: the controller is installed and started"
}

# ---------------------------------------------------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------------------------------------------------

# The bytes of a size such as 4GiB or 500MB, as Arca's templates write sizes. Prints nothing for anything else.
size_bytes() {
  local value=$1 number unit
  number=${value%%[!0-9]*}
  unit=${value#"$number"}
  [ -n "$number" ] || return 1
  case $unit in
    B) echo "$number" ;;
    kB) echo $((number * 1000)) ;;
    MB) echo $((number * 1000 * 1000)) ;;
    GB) echo $((number * 1000 * 1000 * 1000)) ;;
    TB) echo $((number * 1000 * 1000 * 1000 * 1000)) ;;
    KiB) echo $((number * 1024)) ;;
    MiB) echo $((number * 1024 * 1024)) ;;
    GiB) echo $((number * 1024 * 1024 * 1024)) ;;
    TiB) echo $((number * 1024 * 1024 * 1024 * 1024)) ;;
    *) return 1 ;;
  esac
}

mib() { echo $(($1 / 1048576)); }

SLOT_CLASS=()
SLOT_LABELS=()
SLOT_LABELS_JSON=()
SLOT_CPU=()
SLOT_MEM=()
CLASS_NAMES=()

# Reads the settings into the slot table. Every slot has a class: its labels and its size. A setting that cannot be
# used is said in words and the controller does not start.
load_settings() {
  if ! jq -e 'type == "object"' "$POOL_SETTINGS" >/dev/null 2>&1; then
    say "settings: $POOL_SETTINGS is missing or is not a JSON object"
    return 1
  fi
  setting() { jq -r "$1 // empty" "$POOL_SETTINGS"; }
  URL=$(setting .url)
  URL=${URL%/}
  URL=${URL%.git}
  local rest host path owner repo=''
  case $URL in
    https://*) ;;
    *)
      say "settings: url must begin https://, for example https://github.com/example-org"
      return 1
      ;;
  esac
  rest=${URL#https://}
  host=${rest%%/*}
  path=${rest#"$host"}
  path=${path#/}
  owner=${path%%/*}
  [ "$owner" = "$path" ] || repo=${path#*/}
  # The credential is sent to this host, so it is a plain host name and nothing else: letters, digits, dots and
  # dashes, with a port at most. An "@" would make what stands before it a user name and what follows the real
  # host; a "#" or a "?" would end the host early.
  local hostname=${host%%:*} port=''
  [ "$hostname" = "$host" ] || port=${host#*:}
  case $hostname in
    '' | *[!A-Za-z0-9.-]* | [.-]* | *[.-] | *..*)
      say "settings: url: its host must be a plain host name (letters, digits, dots and dashes), with no user, password, query or fragment: the pool's credential is sent to it"
      return 1
      ;;
  esac
  if [ "$hostname" != "$host" ]; then
    case $port in '' | *[!0-9]* | ??????*)
      say "settings: url: its host must be a plain host name (letters, digits, dots and dashes), with no user, password, query or fragment: the pool's credential is sent to it"
      return 1
      ;;
    esac
  fi
  if [ -z "$host" ] || [ -z "$owner" ] || [ -n "${owner//[A-Za-z0-9._-]/}" ] || [ -n "${repo//[A-Za-z0-9._-]/}" ]; then
    say "settings: url names an organisation (https://github.com/example-org) or a repository (https://github.com/example-org/example-repo)"
    return 1
  fi
  if [ -n "$repo" ]; then SCOPE="/repos/$owner/$repo"; else SCOPE="/orgs/$owner"; fi
  # The credential is sent to the API's host, so that host is never taken from a setting alone: github.com has one
  # API, and any other host needs `enterprise` set to true and an API on that host itself.
  local api
  api=$(setting .apiUrl)
  api=${api%/}
  if [ "$host" = github.com ]; then
    if [ -n "$api" ] && [ "$api" != https://api.github.com ]; then
      say "settings: apiUrl: the API of github.com is https://api.github.com and nothing else; leave apiUrl out"
      return 1
    fi
    API_URL=https://api.github.com
  else
    if [ "$(jq -r '.enterprise == true' "$POOL_SETTINGS")" != true ]; then
      say "settings: url is on $host, not on github.com. For a GitHub Enterprise host, set enterprise to true: the pool's credential is then sent to that host"
      return 1
    fi
    : "${api:=https://$host/api/v3}"
    case $api in
      "https://$host" | "https://$host/"* | "https://api.$host" | "https://api.$host/"*) ;;
      *)
        say "settings: apiUrl must begin https:// and be on $host or api.$host: the pool's credential is sent there"
        return 1
        ;;
    esac
    API_URL=$api
  fi
  # The test's fake GitHub. Only root on the machine can set it, in the controller's own unit.
  [ -z "${POOL_API_URL:-}" ] || API_URL=${POOL_API_URL%/}
  GROUP=$(setting .runnerGroupId)
  ALLOW_DEFAULT_GROUP=$(jq -r '.allowDefaultRunnerGroup == true' "$POOL_SETTINGS")
  # A repository has no runner groups: its runners are registered with GitHub's fixed group number.
  [ -z "$repo" ] || GROUP=1
  MAX_SLOTS=$(setting .maxSlots)
  RUNNER_IMAGE=$(setting .runnerImage)
  DOCKER_IMAGE=$(setting .dockerImage)
  local reserve docker_cache dependency_cache
  reserve=$(size_bytes "$(setting .reserveMemory)") || reserve=''
  docker_cache=$(size_bytes "$(setting .dockerCache)") || docker_cache=''
  dependency_cache=$(size_bytes "$(setting .dependencyCache)") || dependency_cache=''
  case "$GROUP$MAX_SLOTS" in '' | *[!0-9]*)
    say "settings: runnerGroupId and maxSlots are whole numbers"
    return 1
    ;;
  esac
  if [ -z "$reserve" ] || [ -z "$docker_cache" ] || [ -z "$dependency_cache" ]; then
    say "settings: reserveMemory, dockerCache and dependencyCache are sizes such as 4GiB"
    return 1
  fi
  if [ -z "$RUNNER_IMAGE" ] || [ -z "$DOCKER_IMAGE" ]; then
    say "settings: runnerImage and dockerImage are required"
    return 1
  fi
  RESERVE=$reserve
  local name labels labels_json cpu memory slots bytes n
  while IFS=$'\t' read -r name labels labels_json cpu memory slots; do
    bytes=$(size_bytes "$memory") || bytes=''
    if [ -z "$name" ] || [ -n "${name//[a-z0-9]/}" ] || [ ${#name} -gt 16 ] ||
      [ -z "$labels" ] || [ -n "${labels//[A-Za-z0-9._,-]/}" ] ||
      [ -z "$cpu" ] || [ -n "${cpu//[0-9]/}" ] || [ "$cpu" -lt 1 ] ||
      [ -z "$slots" ] || [ -n "${slots//[0-9]/}" ] || [ "$slots" -lt 1 ] || [ "$slots" -gt 256 ] ||
      [ -z "$bytes" ]; then
      say "settings: class ${name:-(unnamed)} needs a name of a-z and 0-9, labels of letters, digits, dots, dashes and underscores, a cpu count, a memory size such as 4GiB and a slots count of 1 to 256"
      return 1
    fi
    CLASS_NAMES+=("$name")
    for ((n = 0; n < slots; n++)); do
      SLOT_CLASS+=("$name")
      SLOT_LABELS+=("$labels")
      SLOT_LABELS_JSON+=("$labels_json")
      SLOT_CPU+=("$cpu")
      SLOT_MEM+=("$bytes")
    done
  done < <(jq -r '.classes // [] | .[] | [(.name // ""), ((.labels // []) | map(tostring) | join(",")), ((.labels // []) | map(tostring) | tojson), ((.cpu // "") | tostring), (.memory // ""), ((.slots // "") | tostring)] | @tsv' "$POOL_SETTINGS")
  SLOTS=${#SLOT_CLASS[@]}
  if [ "$SLOTS" -lt 1 ]; then
    say "settings: classes needs at least one class with at least one slot"
    return 1
  fi
  [ "$MAX_SLOTS" -ge 1 ] || MAX_SLOTS=1
  # Each slot's share of the two cache budgets.
  DOCKER_BUDGET=$((docker_cache / SLOTS))
  DEPENDENCY_BUDGET=$((dependency_cache / SLOTS))
  POOL_NAME=${POOL_NAME:-$(hostname 2>/dev/null)}
  POOL_NAME=$(printf '%s' "$POOL_NAME" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9-' '-' | cut -c1-20)
  : "${POOL_NAME:=pool}"
}

# This pool's own id: eight hex digits made once and kept on its disk. Every runner it registers carries it in its
# name, so the pool recognises its own runners on GitHub and never another pool's, whatever that pool is called.
load_pool_id() {
  local file="$POOL_STATE/pool-id"
  POOL_ID=$(cat "$file" 2>/dev/null)
  case $POOL_ID in
    [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) return 0 ;;
  esac
  POOL_ID=$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')
  printf '%s\n' "$POOL_ID" >"$file.new" && mv "$file.new" "$file"
}

# ---------------------------------------------------------------------------------------------------------------------
# GitHub
# ---------------------------------------------------------------------------------------------------------------------

secret_file() { printf '%s/%s' "$POOL_SECRETS" "$1"; }

# Which credential is set: `app`, `token`, or nothing.
credential_kind() {
  if [ -s "$(secret_file GITHUB_APP_CLIENT_ID)" ] && [ -s "$(secret_file GITHUB_APP_PRIVATE_KEY)" ]; then
    echo app
  elif [ -s "$(secret_file GITHUB_TOKEN)" ]; then
    echo token
  fi
}

# One request to GitHub. The token goes to curl on its standard input, never on its command line. On a 2xx answer it
# returns 0 and the body is in $API_OUT; otherwise $API_ERROR says the status and GitHub's own message, which holds no
# credential.
api() {
  local method=$1 path=$2 token=$3 body=${4-}
  API_OUT="$POOL_RUN/tmp/api.$BASHPID"
  API_ERROR=''
  case $token in *[!A-Za-z0-9._~+/=-]* | '')
    API_CODE=000
    API_ERROR='the credential has a character no GitHub token has'
    return 1
    ;;
  esac
  local args=(-sS -X "$method" -o "$API_OUT" -w '%{http_code}' --max-time 30 --connect-timeout 10 -K -)
  [ -n "$body" ] && args+=(--data-binary "$body" -H 'Content-Type: application/json')
  API_CODE=$(printf 'header = "Authorization: Bearer %s"\nheader = "Accept: application/vnd.github+json"\nheader = "X-GitHub-Api-Version: 2022-11-28"\nheader = "User-Agent: arca-runner-pool"\n' "$token" |
    curl "${args[@]}" "$API_URL$path" 2>/dev/null) || API_CODE=000
  case $API_CODE in 2??) return 0 ;; esac
  if [ "$API_CODE" = 000 ]; then
    API_ERROR="GitHub could not be reached at $API_URL"
  else
    API_ERROR="GitHub answered $API_CODE$(jq -r '" (" + ((.message // "no message") | tostring) + ")"' "$API_OUT" 2>/dev/null | tr -d '\n' | cut -c1-200)"
  fi
  return 1
}

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

# The App's private key as a PEM file in $POOL_RUN. A key pasted on one line, with its line breaks as spaces or as the
# two characters \n, is put back into lines.
app_key_file() {
  local source out="$POOL_RUN/app-key.pem" label body
  source=$(secret_file GITHUB_APP_PRIVATE_KEY)
  if openssl pkey -in "$source" -noout >/dev/null 2>&1; then
    printf '%s' "$source"
    return 0
  fi
  label=$(grep -o -m1 'BEGIN [A-Z ]*PRIVATE KEY' "$source" 2>/dev/null | sed 's/^BEGIN //')
  : "${label:=RSA PRIVATE KEY}"
  body=$(sed -e 's/\\n/ /g' -e 's/-----[A-Z ]*-----//g' "$source" | tr -d ' \t\r\n')
  {
    printf -- '-----BEGIN %s-----\n' "$label"
    printf '%s\n' "$body" | fold -w 64
    printf -- '-----END %s-----\n' "$label"
  } >"$out"
  chmod 600 "$out"
  openssl pkey -in "$out" -noout >/dev/null 2>&1 || return 1
  printf '%s' "$out"
}

# A token for GitHub's API, in $TOKEN: an installation token minted from the App's key and kept in $POOL_RUN for
# POOL_TOKEN_TTL seconds, or the fine-grained token itself. The App is used when its two secrets are there, whether or
# not a token is there too. One slot mints at a time. When there is none, $TOKEN_ERROR says why, without a credential.
github_token() {
  TOKEN=''
  TOKEN_ERROR=''
  case $(credential_kind) in
    token)
      TOKEN=$(tr -d ' \t\r\n' <"$(secret_file GITHUB_TOKEN)")
      return 0
      ;;
    app) ;;
    *)
      TOKEN_ERROR='no credential is set'
      return 1
      ;;
  esac
  local out
  out=$(
    exec 9>"$POOL_RUN/token.lock"
    flock 9
    client=$(tr -d ' \t\r\n' <"$(secret_file GITHUB_APP_CLIENT_ID)")
    # The cache is for this client and this key: a new secret is used from the next token on.
    tag=$(cat "$(secret_file GITHUB_APP_CLIENT_ID)" "$(secret_file GITHUB_APP_PRIVATE_KEY)" | sha256sum | cut -c1-32)
    if [ -s "$POOL_RUN/token" ]; then
      read -r held_tag held_until held_token <"$POOL_RUN/token"
      if [ "$held_tag" = "$tag" ] && [ "$held_until" -gt "$(now)" ] 2>/dev/null; then
        printf 'ok %s' "$held_token"
        exit 0
      fi
    fi
    case $client in '' | *[!A-Za-z0-9._-]*)
      printf 'no GITHUB_APP_CLIENT_ID is not a client ID (the App page shows it; it begins Iv)'
      exit 0
      ;;
    esac
    if ! key=$(app_key_file); then
      printf 'no GITHUB_APP_PRIVATE_KEY is not a private key in PEM form'
      exit 0
    fi
    at=$(now)
    head=$(printf '{"alg":"RS256","typ":"JWT"}' | b64url)
    claims=$(printf '{"iat":%d,"exp":%d,"iss":"%s"}' $((at - 60)) $((at + 540)) "$client" | b64url)
    if ! signature=$(printf '%s.%s' "$head" "$claims" | openssl dgst -sha256 -sign "$key" -binary 2>/dev/null | b64url) || [ -z "$signature" ]; then
      printf 'no GITHUB_APP_PRIVATE_KEY could not sign'
      exit 0
    fi
    jwt="$head.$claims.$signature"
    if ! api GET "$SCOPE/installation" "$jwt"; then
      printf 'no the App is not installed on %s, or its client ID or key is wrong: %s' "$URL" "$API_ERROR"
      exit 0
    fi
    installation=$(jq -r '.id // empty' "$API_OUT")
    case $installation in '' | *[!0-9]*)
      printf 'no GitHub named no installation of the App on %s' "$URL"
      exit 0
      ;;
    esac
    if ! api POST "/app/installations/$installation/access_tokens" "$jwt"; then
      printf 'no an installation token could not be made: %s' "$API_ERROR"
      exit 0
    fi
    token=$(jq -r '.token // empty' "$API_OUT")
    rm -f "$API_OUT"
    if [ -z "$token" ]; then
      printf 'no GitHub answered without a token'
      exit 0
    fi
    # Installation tokens last an hour; this one is used for POOL_TOKEN_TTL seconds, fifty minutes.
    (umask 077 && printf '%s %s %s\n' "$tag" $((at + POOL_TOKEN_TTL)) "$token" >"$POOL_RUN/token.new") &&
      mv "$POOL_RUN/token.new" "$POOL_RUN/token"
    printf 'ok %s' "$token"
  )
  case $out in
    'ok '*)
      TOKEN=${out#ok }
      return 0
      ;;
  esac
  TOKEN_ERROR=${out#no }
  : "${TOKEN_ERROR:=a token could not be made}"
  return 1
}

# ---------------------------------------------------------------------------------------------------------------------
# One slot
# ---------------------------------------------------------------------------------------------------------------------

slot_dir() { printf '%s/slots/%s' "$POOL_STATE" "$1"; }
slot_slice() { printf 'arcapool-s%s.slice' "$1"; }
slot_cgroup() { printf '%s/arcapool.slice/arcapool-s%s.slice' "$POOL_CGROUP" "$1"; }

# The slot's state for the status block: its phase, when it began, and a few words.
set_state() {
  local slot=$1 phase=$2
  shift 2
  printf '%s\t%s\t%s\n' "$phase" "$(now)" "$*" >"$POOL_RUN/slots/$slot.new" &&
    mv "$POOL_RUN/slots/$slot.new" "$POOL_RUN/slots/$slot"
}

# Text from a job, made safe for one line of a console and one field of a tab-separated file.
plain() { printf '%s' "$1" | tr -c 'A-Za-z0-9 ._:/()#@+,=-' '?' | cut -c1-120; }

# When the job a runner's output names began, as seconds: the runner prints the time before "Running job:". Now, when
# the time cannot be read.
job_began() {
  local stamp at
  stamp=$(printf '%s\n' "$1" | sed -n 's/.*\([0-9]\{4\}-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9]\)Z: Running job: .*/\1/p' | head -n1)
  at=$(date -u -d "$stamp UTC" +%s 2>/dev/null) || at=''
  case $at in '' | *[!0-9]*) now ;; *) echo "$at" ;; esac
}

# A failure that is the pool's own, not a job's: it is counted, said, and makes every slot pause longer before the
# next registration, up to POOL_BACKOFF_MAX.
slot_failed() {
  local slot=$1 pause
  shift
  printf x >>"$POOL_RUN/failures"
  pause=$(cat "$POOL_RUN/backoff" 2>/dev/null)
  case $pause in '' | *[!0-9]*) pause=0 ;; esac
  pause=$((pause == 0 ? 5 : pause * 2))
  [ "$pause" -gt "$POOL_BACKOFF_MAX" ] && pause=$POOL_BACKOFF_MAX
  echo "$pause" >"$POOL_RUN/backoff"
  echo $(($(now) + pause)) >"$POOL_RUN/backoff-until"
  set_state "$slot" failed "$*"
  say "slot $slot (${SLOT_CLASS[$slot]}) failed: $*. Slots start again in $pause seconds"
}

# Removes a slot's containers and what the job left: the per-job cleanup. The Docker layer cache and the dependency
# caches stay, unless the slot's share of their budget is passed, and then they are emptied.
slot_reap() {
  local slot=$1 dir runner daemon used
  dir=$(slot_dir "$slot")
  runner="$POOL_NAME-s$slot-run"
  daemon="$POOL_NAME-s$slot-dind"
  set_state "$slot" cleaning
  # Inside the slot's own Docker daemon: the job's containers, volumes and networks go; images and build cache stay.
  docker exec "$daemon" sh -c 'c=$(docker ps -aq); [ -z "$c" ] || docker rm -fv $c; docker volume prune -af; docker network prune -f' >/dev/null 2>&1
  used=$(du -sb "$dir/docker" 2>/dev/null | cut -f1)
  if [ "${used:-0}" -gt "$DOCKER_BUDGET" ]; then
    docker exec "$daemon" sh -c 'docker system prune -af --volumes; docker builder prune -af' >/dev/null 2>&1
    say "slot $slot: its Docker cache passed $(mib "$DOCKER_BUDGET") MiB and was emptied"
  fi
  docker rm -fv "$runner" "$daemon" >/dev/null 2>&1
  # Whatever is left in the slot's slice ends with it, and its memory peak starts from nothing for the next job.
  systemctl stop "$(slot_slice "$slot")" >/dev/null 2>&1
  rm -rf "${dir:?}/work" "${dir:?}/sock" "${POOL_RUN:?}/jit/$slot"
  unlayer_home "$slot"
  used=$(du -sb "$dir/toolcache" "$dir/cache" 2>/dev/null | awk '{ total += $1 } END { print total + 0 }')
  if [ "${used:-0}" -gt "$DEPENDENCY_BUDGET" ]; then
    find "${dir:?}/toolcache" "${dir:?}/cache" -mindepth 1 -delete 2>/dev/null
    say "slot $slot: its dependency cache passed $(mib "$DEPENDENCY_BUDGET") MiB and was emptied"
  fi
}

# One life of a slot: register a fresh runner, run it, record its job, clean up. It ends when the runner has gone, so
# the controller starts the slot again with a new registration.
slot_cycle() {
  local slot=$1
  local class=${SLOT_CLASS[$slot]} dir runner daemon slice token name body jit runner_id tries state code
  local started busy_since='' job='' result='' peak cpu seconds finished cgroup updating='' layered='' home=() lines
  ABORT=''
  trap 'ABORT=1' TERM INT
  dir=$(slot_dir "$slot")
  runner="$POOL_NAME-s$slot-run"
  daemon="$POOL_NAME-s$slot-dind"
  slice=$(slot_slice "$slot")
  cgroup=$(slot_cgroup "$slot")
  set_state "$slot" registering

  if ! github_token; then
    slot_failed "$slot" "$TOKEN_ERROR"
    return 0
  fi
  token=$TOKEN
  TOKEN=''
  name="$POOL_NAME-$POOL_ID-$class-$slot-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
  body=$(jq -nc --arg name "$name" --argjson group "$GROUP" --argjson labels "${SLOT_LABELS_JSON[$slot]}" \
    '{name: $name, runner_group_id: $group, labels: $labels, work_folder: "_work"}')
  if ! api POST "$SCOPE/actions/runners/generate-jitconfig" "$token" "$body"; then
    # A token GitHub no longer takes is not kept.
    [ "$API_CODE" = 401 ] && rm -f "$POOL_RUN/token"
    slot_failed "$slot" "the runner was not registered: $API_ERROR"
    return 0
  fi
  jit=$(jq -r '.encoded_jit_config // empty' "$API_OUT")
  runner_id=$(jq -r '.runner.id // empty' "$API_OUT")
  rm -f "$API_OUT"
  if [ -z "$jit" ]; then
    slot_failed "$slot" "GitHub registered the runner without a just-in-time configuration"
    return 0
  fi
  rm -f "$POOL_RUN/backoff" "$POOL_RUN/backoff-until"

  # A fresh work directory and socket directory for this runner. The caches are the slot's own and are kept.
  rm -rf "${dir:?}/work" "${dir:?}/sock"
  mkdir -p "$dir/work" "$dir/sock" "$dir/docker" "$dir/toolcache" "$dir/cache"
  chmod 755 "$dir" "$dir/work" "$dir/sock" "$dir/toolcache" "$dir/cache"
  if [ "$(id -u)" = 0 ]; then chown "$RUNNER_UID:$DOCKER_GID" "$dir/work" "$dir/toolcache" "$dir/cache"; fi
  set_state "$slot" starting "$name"
  docker rm -fv "$runner" "$daemon" >/dev/null 2>&1
  # The runner's files for this job. Layered: one fresh copy that the runner writes and the daemon reads, both at the
  # runner's own path. Plain: the runner keeps its image's own files, and the daemon has the pinned bundled tools,
  # read-only. Either way the runner's tools are no mount of their own, so the runner can replace them when GitHub
  # tells it to update itself.
  if layer_home "$slot"; then
    layered=1
    home=(-v "$dir/layer/home:/home/runner:ro")
  else
    home=(-v "$EXTERNALS:/home/runner/externals:ro")
  fi
  # The slot's own Docker daemon. Its data directory is the slot's layer cache. It sees the runner's bundled tools at
  # the runner's own path, because a job that runs in a container has them mounted from there by this daemon.
  if ! docker run -d --name "$daemon" --label "arca.pool=$POOL_NAME" --privileged --cgroup-parent "$slice" \
    -v "$dir/sock:/var/run" "${home[@]}" -v "$dir/work:/home/runner/_work" \
    -v "$dir/toolcache:/opt/hostedtoolcache" -v "$dir/docker:/var/lib/docker" \
    "$DOCKER_IMAGE" dockerd --host=unix:///var/run/docker.sock --group="$DOCKER_GID" >/dev/null 2>&1; then
    slot_abandon "$slot" "$runner_id"
    slot_failed "$slot" "the slot's Docker daemon could not be started"
    return 0
  fi
  tries=0
  until docker exec "$daemon" docker info >/dev/null 2>&1; do
    tries=$((tries + 1))
    if [ -n "$ABORT" ] || [ "$tries" -gt "$POOL_READY_WAIT" ]; then
      slot_abandon "$slot" "$runner_id"
      [ -n "$ABORT" ] || slot_failed "$slot" "the slot's Docker daemon did not come up within $POOL_READY_WAIT seconds"
      return 0
    fi
    nap 1
  done
  # The runner: a fresh container from the pinned image, sharing the daemon's network so a job's service containers
  # answer on localhost. Its own files, the bundled tools among them, are the image's and are writable and not a mount,
  # so the runner can replace them when GitHub tells it to update itself. Its just-in-time configuration is a file on
  # /run that the container's command reads into the runner's environment: Docker's record of the container holds the
  # path, never the configuration.
  mkdir -p "$POOL_RUN/jit"
  chmod 700 "$POOL_RUN/jit"
  (umask 077 && printf '%s' "$jit" >"$POOL_RUN/jit/$slot")
  chmod 400 "$POOL_RUN/jit/$slot"
  if [ "$(id -u)" = 0 ]; then chown "$RUNNER_UID:$DOCKER_GID" "$POOL_RUN/jit/$slot"; fi
  jit=''
  home=()
  [ -z "$layered" ] || home=(-v "$dir/layer/home:/home/runner")
  # shellcheck disable=SC2016
  if ! docker run -d --name "$runner" --label "arca.pool=$POOL_NAME" \
    --cgroup-parent "$slice" --network "container:$daemon" \
    -e DOCKER_HOST=unix:///var/run/docker.sock \
    -e RUNNER_TOOL_CACHE=/opt/hostedtoolcache -e POOL_CACHE_DIR=/var/cache/pool \
    -v "$POOL_RUN/jit/$slot:/run/arca/jitconfig:ro" \
    -v "$dir/sock:/var/run" "${home[@]}" -v "$dir/work:/home/runner/_work" \
    -v "$dir/toolcache:/opt/hostedtoolcache" -v "$dir/cache:/var/cache/pool" \
    "$RUNNER_IMAGE" /bin/bash -c 'ACTIONS_RUNNER_INPUT_JITCONFIG=$(cat /run/arca/jitconfig) && export ACTIONS_RUNNER_INPUT_JITCONFIG && exec /home/runner/run.sh' >/dev/null 2>&1; then
    slot_abandon "$slot" "$runner_id"
    slot_failed "$slot" "the runner container could not be started"
    return 0
  fi
  started=$(now)
  set_state "$slot" idle "$name"

  while [ -z "$ABORT" ]; do
    state=$(docker inspect -f '{{.State.Running}} {{.State.ExitCode}}' "$runner" 2>/dev/null) || state='false 255'
    [ "${state%% *}" = true ] || break
    # A runner that has taken its job says so in its own output, with the job's name and the time: "Running job:".
    # That line is what marks the slot busy. (The slot was once asked for the runner's processes instead, in a form
    # Docker refuses, so a running job was shown as idle.) The output is read only until the job has begun.
    lines=''
    [ -n "$busy_since" ] || lines=$(docker logs "$runner" 2>&1)
    case $lines in *'Running job: '*)
      busy_since=$(job_began "$lines")
      job=$(plain "$(printf '%s\n' "$lines" | sed -n 's/.*Running job: //p' | head -n1)")
      set_state "$slot" busy "$name ${job:+job \"$job\"}"
      ;;
    esac
    # GitHub tells a runner older than its newest release to update itself before it takes a job. It does so inside
    # its own container; the pool says it once per runner, because every fresh runner will until the pin moves.
    if [ -z "$updating" ] && [ -z "$busy_since" ] && [[ $lines == *'Runner update in progress'* ]]; then
      updating=1
      printf x >>"$POOL_RUN/updates"
      set_state "$slot" updating "$name"
      say "slot $slot: GitHub told this runner to update itself, because the pool's pinned runner is older than GitHub's newest. It updates inside its own container and then takes its job; every new runner of this pool does the same, at the cost of a download each, until a pool is created with a newer runnerImage"
    fi
    # The runner's own wrapper says when it starts the updated runner: from then on the update is over.
    if [ "$updating" = 1 ] && [[ $lines == *'Restarting runner'* ]]; then
      updating=finished
      [ -n "$busy_since" ] || set_state "$slot" idle "$name"
    fi
    nap "$POOL_POLL"
  done

  if [ -n "$ABORT" ]; then
    # The controller is stopping. A runner that has no job is taken off GitHub; one with a job is lost with it.
    [ -n "$busy_since" ] || slot_abandon "$slot" "$runner_id"
    docker rm -fv "$runner" "$daemon" >/dev/null 2>&1
    systemctl stop "$slice" >/dev/null 2>&1
    rm -f "$POOL_RUN/jit/$slot"
    unlayer_home "$slot"
    return 0
  fi

  code=${state##* }
  finished=$(now)
  # A job shorter than one look is still found in the runner's own lines, with the time it began.
  lines=$(docker logs "$runner" 2>&1)
  if [ -z "$busy_since" ] && [[ $lines == *'Running job: '* ]]; then
    busy_since=$(job_began "$lines")
    job=$(plain "$(printf '%s\n' "$lines" | sed -n 's/.*Running job: //p' | head -n1)")
  fi
  result=$(printf '%s\n' "$lines" | sed -n 's/.*completed with result: \([A-Za-z]*\).*/\1/p' | tail -n1)
  lines=''
  peak=$(cat "$cgroup/memory.peak" 2>/dev/null)
  cpu=$(awk '$1 == "usage_usec" { print $2 }' "$cgroup/cpu.stat" 2>/dev/null)
  case $peak in '' | *[!0-9]*) peak=0 ;; esac
  case $cpu in '' | *[!0-9]*) cpu=0 ;; esac
  if [ -n "$busy_since" ] || [ -n "$job" ]; then
    seconds=$((finished - ${busy_since:-$started}))
    [ "$seconds" -ge 0 ] || seconds=0
    : "${result:=unknown}"
    : "${job:=unknown}"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s.%s\t%s\n' \
      "$(date -u -d "@$finished" +%Y-%m-%dT%H:%M:%SZ)" "$slot" "$class" "${SLOT_LABELS[$slot]}" "$job" "$result" \
      "$seconds" "$peak" $((cpu / 1000000)) $(((cpu % 1000000) / 100000)) "$name" >>"$POOL_LOGS/jobs.tsv"
    printf x >>"$POOL_RUN/jobs"
    say "job slot=$slot class=$class labels=${SLOT_LABELS[$slot]} name=\"$job\" result=$result seconds=$seconds peak_memory_mib=$(mib "$peak") cpu_seconds=$((cpu / 1000000)).$(((cpu % 1000000) / 100000)) runner=$name"
    slot_reap "$slot"
  else
    slot_reap "$slot"
    slot_abandon "$slot" "$runner_id"
    case $updating in
      1) slot_failed "$slot" "its runner stopped (exit $code) while it was updating itself, before it took a job" ;;
      finished) slot_failed "$slot" "its runner stopped (exit $code) after it had updated itself, before it took a job" ;;
      *)
        slot_failed "$slot" "its runner stopped (exit $code) before it took a job"
        # A runner that never got as far as a job with its files layered: the next ones start plain, so one bad
        # start cannot keep the pool from running. (A runner GitHub removed gives the same sign; the cost of
        # taking it for this is only the pinned tools in container jobs after an update.)
        [ -z "$layered" ] || plain_home "A runner stopped before its first job with its files layered"
        ;;
    esac
  fi
  return 0
}

# Takes a runner that will never run off GitHub, so no dead registration is left. The token is asked for here and not
# kept from the slot's start: an idle runner can be older than the token it was registered with. Best effort: GitHub
# removes an offline ephemeral runner by itself after a day.
slot_abandon() {
  local slot=$1 runner_id=$2
  docker rm -fv "$POOL_NAME-s$slot-run" "$POOL_NAME-s$slot-dind" >/dev/null 2>&1
  systemctl stop "$(slot_slice "$slot")" >/dev/null 2>&1
  rm -f "$POOL_RUN/jit/$slot"
  unlayer_home "$slot"
  case $runner_id in '' | *[!0-9]*) return 0 ;; esac
  github_token || return 0
  api DELETE "$SCOPE/actions/runners/$runner_id" "$TOKEN" || true
  TOKEN=''
  rm -f "$API_OUT"
}

# ---------------------------------------------------------------------------------------------------------------------
# The controller
# ---------------------------------------------------------------------------------------------------------------------

meminfo_bytes() { awk -v key="$1:" '$1 == key { printf "%d", $2 * 1024 }' "$POOL_MEMINFO"; }

# Each slot's slice: memory is capped at the slot's size and never swapped, so a slot cannot take another's, and CPU is
# a ceiling that slots share.
write_slices() {
  local slot
  mkdir -p "$POOL_UNITS"
  for ((slot = 0; slot < SLOTS; slot++)); do
    cat >"$POOL_UNITS/$(slot_slice "$slot")" <<UNIT
[Unit]
Description=Arca runner pool slot $slot (${SLOT_CLASS[$slot]})

[Slice]
MemoryMax=${SLOT_MEM[$slot]}
MemorySwapMax=0
CPUQuota=$((SLOT_CPU[slot] * 100))%
UNIT
  done
  systemctl daemon-reload >/dev/null 2>&1
}

# Pulls the two pinned images and copies the runner's bundled tools out once, for every slot to mount read-only.
prepare_images() {
  local image tag
  for image in "$RUNNER_IMAGE" "$DOCKER_IMAGE"; do
    until docker image inspect "$image" >/dev/null 2>&1 || docker pull -q "$image" >/dev/null 2>&1; do
      [ -n "$STOP" ] && return 1
      say "the image $image could not be pulled; trying again in 30 seconds"
      nap 30
    done
  done
  # The runner's own files, copied out of the pinned image once, with their owners: every runner starts from them.
  tag=$(printf '%s' "$RUNNER_IMAGE" | sha256sum | cut -c1-16)
  RUNNER_HOME="$POOL_STATE/runner-home/$tag"
  EXTERNALS="$RUNNER_HOME/externals"
  if [ ! -e "$RUNNER_HOME.complete" ]; then
    rm -rf "${RUNNER_HOME:?}"
    mkdir -p "$RUNNER_HOME"
    docker rm -f "$POOL_NAME-externals" >/dev/null 2>&1
    if ! docker create --name "$POOL_NAME-externals" "$RUNNER_IMAGE" >/dev/null 2>&1 ||
      ! docker cp -a "$POOL_NAME-externals:/home/runner/." "$RUNNER_HOME/" >/dev/null 2>&1; then
      docker rm -f "$POOL_NAME-externals" >/dev/null 2>&1
      say "the runner's files could not be copied out of $RUNNER_IMAGE"
      return 1
    fi
    docker rm -f "$POOL_NAME-externals" >/dev/null 2>&1
    touch "$RUNNER_HOME.complete"
  fi
}

# Gives a slot's runner and its Docker daemon one fresh, writable copy of the runner's files to share: the pinned
# image's files as the read-only lower layer of an overlay, and an empty upper layer that lives for one job. Both
# containers mount it at the runner's own path, so when a runner updates itself the daemon sees the new bundled tools
# too, and a job that runs in a container gets the tools of the runner that started it. Returns 1 when the machine
# cannot layer (the slot then starts with the image's own files, as a pool without this did), and says so once.
layer_home() {
  local slot=$1 dir
  dir=$(slot_dir "$slot")
  [ ! -e "$POOL_RUN/plain-home" ] || return 1
  unlayer_home "$slot"
  mkdir -p "$dir/layer/home" "$dir/layer/upper" "$dir/layer/work"
  # The runner's home is its own and open, as the image has it.
  chmod 777 "$dir/layer/upper"
  if [ "$(id -u)" = 0 ]; then chown "$RUNNER_UID:$DOCKER_GID" "$dir/layer/upper"; fi
  if mount -t overlay overlay -o "lowerdir=$RUNNER_HOME,upperdir=$dir/layer/upper,workdir=$dir/layer/work" "$dir/layer/home" >/dev/null 2>&1 &&
    mkdir -p "$dir/layer/home/_work"; then
    return 0
  fi
  unlayer_home "$slot"
  plain_home "The runner's files could not be layered on this machine (overlay mount)"
  return 1
}

# Takes a slot's layered copy away, with everything the job's runner wrote into it.
unlayer_home() {
  local dir
  dir=$(slot_dir "$1")
  umount -l "$dir/layer/home" >/dev/null 2>&1
  rm -rf "${dir:?}/layer"
}

# From now on every runner starts with the image's own files, and the daemon with the pinned tools: said once.
plain_home() {
  [ ! -e "$POOL_RUN/plain-home" ] || return 0
  : >"$POOL_RUN/plain-home"
  say "$1. Runners start with their image's own files from now on; after a runner updates itself, a job that runs in a container (container:) still gets the pinned release's bundled tools"
}

# The containers an earlier controller left on this machine.
clean_containers() {
  local ids
  ids=$(docker ps -aq --filter "label=arca.pool=$POOL_NAME" 2>/dev/null)
  # shellcheck disable=SC2086
  [ -z "$ids" ] || docker rm -fv $ids >/dev/null 2>&1
  systemctl stop 'arcapool-s*.slice' >/dev/null 2>&1
  local slot
  for ((slot = 0; slot < SLOTS; slot++)); do unlayer_home "$slot"; done
}

# The runners an earlier controller of this pool left on GitHub: those GitHub lists as offline whose whole name is one
# this pool gives (its name, its own id, a class, a slot and eight hex digits). A runner of another pool is never
# matched, also when that pool's name begins with this one's. It runs before any slot starts, so no runner of this
# controller is among them.
clean_runners() {
  local token=$1 page id listed stale
  for page in 1 2 3 4 5 6 7 8 9 10; do
    api GET "$SCOPE/actions/runners?per_page=100&page=$page" "$token" || break
    listed=$(jq -r '.runners // [] | length' "$API_OUT")
    stale=$(jq -r --arg own "^$POOL_NAME-$POOL_ID-[a-z0-9]+-[0-9]+-[0-9a-f]{8}\$" '.runners // [] | .[] | select((.name // "") | test($own)) | select(.status == "offline") | .id' "$API_OUT")
    for id in $stale; do
      case $id in *[!0-9]*) continue ;; esac
      api DELETE "$SCOPE/actions/runners/$id" "$token" || true
    done
    [ "$listed" = 100 ] || break
  done
  rm -f "$API_OUT"
}

# The organisation's runner groups, as words for a refusal: each with its id.
runner_groups() {
  local token=$1
  api GET "$SCOPE/actions/runner-groups?per_page=100" "$token" || return 1
  jq -r '[.runner_groups // [] | .[] | "\(.id) \"\(.name // "" | gsub("[^A-Za-z0-9 ._-]"; "?"))\"" + (if .default then " (the default group)" else "" end)] | join(", ")' "$API_OUT"
}

# Whether the pool may register runners where its settings say. For an organisation that is its runner group: the
# default group is refused unless the settings say allowDefaultRunnerGroup, since every repository that group admits
# could send its jobs here, and a job here can reach the pool's credential. For one repository there is no group, and
# what is read is whether the repository is public. Returns 0 to go on, 1 for a refusal that only another pool (other
# settings) or a change on GitHub ends, and 2 when GitHub could not be asked or did not answer what was asked.
# $GROUP_WORDS says what was found; $GROUP_NOTE, for the status block, changes only when GitHub answered.
check_runner_group() {
  local token=$1 name default visibility public groups note
  GROUP_WORDS=''
  case $SCOPE in /repos/*)
    if ! api GET "$SCOPE" "$token"; then
      GROUP_WORDS="the repository could not be read: $API_ERROR"
      return 2
    fi
    # The answer must be about a repository: it says whether it is private, as true or false.
    case $(jq -r '.private | type' "$API_OUT" 2>/dev/null) in
      boolean) ;;
      *)
        GROUP_WORDS="GitHub's answer for the repository does not say whether it is private"
        return 2
        ;;
    esac
    if [ "$(jq -r '.private' "$API_OUT")" = true ]; then
      GROUP_NOTE='one private repository (a repository has no runner groups)'
    else
      GROUP_NOTE='one repository, WARNING: it is public, so a pull request from anyone, once it is approved to run, runs code in this pool'
    fi
    return 0
    ;;
  esac
  case $GROUP in '' | *[!0-9]*)
    groups=$(runner_groups "$token") || groups='(they could not be listed)'
    GROUP_WORDS="settings: runnerGroupId is not set. Create a runner group for this pool on GitHub, limited to selected repositories and with public repositories off, and create the pool with its id as runnerGroupId. The organisation's runner groups are: $groups"
    GROUP_NOTE='REFUSED: no runner group is set'
    return 1
    ;;
  esac
  if ! api GET "$SCOPE/actions/runner-groups/$GROUP" "$token"; then
    if [ "$API_CODE" != 404 ]; then
      GROUP_WORDS="the runner group $GROUP could not be read: $API_ERROR"
      return 2
    fi
    groups=$(runner_groups "$token") || groups='(they could not be listed)'
    GROUP_WORDS="settings: runnerGroupId $GROUP is not a runner group of $URL. Its runner groups are: $groups"
    GROUP_NOTE="REFUSED: runner group $GROUP does not exist"
    return 1
  fi
  # The answer must be about this group and must say whether it is the default one, as true or false. An answer
  # without either is no ground to start on.
  if [ "$(jq -r --argjson id "$GROUP" '(.id == $id) and ((.default | type) == "boolean")' "$API_OUT" 2>/dev/null)" != true ]; then
    GROUP_WORDS="GitHub's answer for runner group $GROUP does not name that group or does not say whether it is the default one"
    return 2
  fi
  name=$(jq -r '.name // "" | gsub("[^A-Za-z0-9 ._-]"; "?")' "$API_OUT")
  default=$(jq -r '.default' "$API_OUT")
  visibility=$(jq -r '.visibility // "unknown" | gsub("[^a-z]"; "?")' "$API_OUT")
  public=$(jq -r '.allows_public_repositories == true' "$API_OUT")
  if [ "$default" = true ] && [ "$ALLOW_DEFAULT_GROUP" != true ]; then
    groups=$(runner_groups "$token") || groups='(they could not be listed)'
    GROUP_WORDS="the pool does not start on the organisation's default runner group ($GROUP \"$name\"): every repository that group admits could send its jobs here, and a job here can reach the pool's GitHub credential. Create a runner group for this pool, limited to selected repositories and with public repositories off, and create the pool with its id as runnerGroupId. The organisation's runner groups are: $groups"
    GROUP_NOTE="REFUSED: runner group $GROUP \"$name\" is the organisation's default group"
    return 1
  fi
  note="runner group $GROUP \"$name\""
  case $visibility in
    selected) note="$note, selected repositories only" ;;
    *) note="$note, WARNING: open to every repository of the organisation ($visibility)" ;;
  esac
  if [ "$public" = true ]; then
    note="$note, WARNING: public repositories admitted, so a pull request from anyone, once it is approved to run, runs code in this pool"
  else
    note="$note, public repositories off"
  fi
  [ "$default" != true ] || note="$note, WARNING: the default group, allowed by allowDefaultRunnerGroup"
  GROUP_NOTE=$note
  return 0
}

# Sets jobs.tsv aside as jobs.tsv.1, replacing the one set aside before, once it passes POOL_JOBS_MAX. The header is
# appended to the new file, never written over it: a slot may have put a job's line there in between.
rotate_jobs() {
  local size
  size=$(stat -c %s "$POOL_LOGS/jobs.tsv" 2>/dev/null || echo 0)
  [ "$size" -gt "$POOL_JOBS_MAX" ] || return 0
  mv "$POOL_LOGS/jobs.tsv" "$POOL_LOGS/jobs.tsv.1"
  printf '%s\n' "$JOBS_HEADER" >>"$POOL_LOGS/jobs.tsv"
}

count_of() { stat -c %s "$POOL_RUN/$1" 2>/dev/null || echo 0; }

# The status block: one line per slot, one per class with what its jobs used, and the totals.
print_status() {
  local slot phase since words busy=0 idle=0 starting=0 queued=0 reserved=0 at line class
  at=$(now)
  {
    for ((slot = 0; slot < SLOTS; slot++)); do
      if [ -n "${PIDS[$slot]:-}" ]; then
        { IFS=$'\t' read -r phase since words <"$POOL_RUN/slots/$slot"; } 2>/dev/null || {
          phase=starting
          since=$at
          words=''
        }
        reserved=$((reserved + SLOT_MEM[slot]))
        case $phase in
          busy) busy=$((busy + 1)) ;;
          idle | updating) idle=$((idle + 1)) ;;
          *) starting=$((starting + 1)) ;;
        esac
        line="$phase $((at - since))s${words:+ $words}"
      else
        queued=$((queued + 1))
        line="queued (${WAIT[$slot]:-waiting to start})"
      fi
      say "slot $slot ${SLOT_CLASS[$slot]} $(mib "${SLOT_MEM[$slot]}")MiB/${SLOT_CPU[$slot]}cpu $line"
    done
    # What the last jobs of each class used, beside the class's size: the numbers to tune the sizes from.
    for class in "${CLASS_NAMES[@]}"; do
      cat "$POOL_LOGS/jobs.tsv.1" "$POOL_LOGS/jobs.tsv" 2>/dev/null | tail -n 2000 | awk -F'\t' -v class="$class" '
        $3 == class { n++; s += $7; if ($8 > peak) peak = $8; mem += $8; cpu += $9 }
        END { if (n) printf "arca-runner-pool: class %s last %d jobs: seconds avg %d, peak memory avg %d MiB max %d MiB, cpu-seconds avg %d, cpus used avg %.1f\n", class, n, s / n, mem / n / 1048576, peak / 1048576, cpu / n, (s ? cpu / s : 0) }'
    done
    # The last finished jobs again, so they stay within reach of service_logs however many status blocks follow.
    tail -n "$POOL_STATUS_JOBS" "$POOL_LOGS/jobs.tsv" 2>/dev/null | awk -F'\t' '
      $1 != "finished" { printf "arca-runner-pool: recent job finished=%s slot=%s class=%s name=\"%s\" result=%s seconds=%s peak_memory_mib=%d cpu_seconds=%s\n", $1, $2, $3, $5, $6, $7, $8 / 1048576, $9 }'
    say "pool $POOL_NAME id $POOL_ID for $URL, $GROUP_NOTE"
    say "totals busy=$busy idle=$idle starting=$starting queued=$queued jobs_done=$(count_of jobs) failures=$(count_of failures) runner_updates=$(count_of updates) memory_reserved=$(mib "$reserved")MiB of $(mib "$BUDGET")MiB slots_cap=$MAX_SLOTS"
  } | tee "$POOL_LOGS/status.txt.new"
  mv "$POOL_LOGS/status.txt.new" "$POOL_LOGS/status.txt"
}

run_pool() {
  STOP=''
  trap 'STOP=1' TERM INT
  load_settings || {
    say "the pool does not start until its settings are right: they were fixed when the machine was created, so create the pool again with the right ones"
    # Stay up, so the reason stays on the console instead of a restart loop.
    while [ -z "$STOP" ]; do nap 300; done
    return 1
  }
  mkdir -p "$POOL_STATE/slots" "$POOL_LOGS" "$POOL_RUN/slots" "$POOL_RUN/tmp" "$POOL_RUN/jit"
  chmod 700 "$POOL_RUN" "$POOL_RUN/tmp" "$POOL_RUN/jit"
  rm -f "${POOL_RUN:?}"/slots/* "${POOL_RUN:?}"/jit/* "${POOL_RUN:?}/jobs" "${POOL_RUN:?}/failures" "${POOL_RUN:?}/updates" "${POOL_RUN:?}/backoff" "${POOL_RUN:?}/backoff-until" "${POOL_RUN:?}/plain-home" "${POOL_RUN:?}"/tmp/*
  load_pool_id
  JOBS_HEADER=$(printf 'finished\tslot\tclass\tlabels\tjob\tresult\tseconds\tpeak_memory_bytes\tcpu_seconds\trunner')
  [ -s "$POOL_LOGS/jobs.tsv" ] || printf '%s\n' "$JOBS_HEADER" >"$POOL_LOGS/jobs.tsv"
  local total
  total=$(meminfo_bytes MemTotal)
  BUDGET=$((${total:-0} - RESERVE))
  GROUP_NOTE=''
  GROUP_WORDS=''
  say "pool $POOL_NAME id $POOL_ID for $URL: $SLOTS slots in ${#CLASS_NAMES[@]} classes, at most $MAX_SLOTS at once, $(mib "$BUDGET") MiB of memory for slots"
  write_slices

  local waited=0
  while [ -z "$STOP" ] && [ -z "$(credential_kind)" ]; do
    if [ $((waited % 60)) = 0 ]; then
      say "waiting for a credential: set GITHUB_APP_CLIENT_ID and GITHUB_APP_PRIVATE_KEY, or GITHUB_TOKEN, with service_secret_set, then run instance_start"
    fi
    nap 5
    waited=$((waited + 5))
  done
  [ -z "$STOP" ] || return 0
  if [ "$(credential_kind)" = app ] && [ -s "$(secret_file GITHUB_TOKEN)" ]; then
    say "signing in as the GitHub App: GITHUB_TOKEN is set too and is not used"
  fi
  until prepare_images; do
    [ -z "$STOP" ] || return 0
    nap 30
  done
  clean_containers
  # No slot starts before GitHub takes the credential: a wrong one is said here, once per pause, not once per slot.
  local pause=0
  until github_token; do
    [ -z "$STOP" ] || return 0
    printf x >>"$POOL_RUN/failures"
    pause=$((pause == 0 ? 5 : pause * 2))
    [ "$pause" -gt "$POOL_BACKOFF_MAX" ] && pause=$POOL_BACKOFF_MAX
    say "GitHub sign-in failed: $TOKEN_ERROR. Trying again in $pause seconds"
    nap "$pause"
  done
  [ -z "$STOP" ] || return 0
  # Nor before its runner group is one a pool may use. A refusal is told again every POOL_REFUSE_EVERY seconds, so it
  # stays on the console; only a pool created with other settings ends it.
  local verdict
  while :; do
    github_token || {
      printf x >>"$POOL_RUN/failures"
      say "GitHub sign-in failed: $TOKEN_ERROR. Trying again in $POOL_BACKOFF_MAX seconds"
      nap "$POOL_BACKOFF_MAX"
      [ -z "$STOP" ] || return 0
      continue
    }
    check_runner_group "$TOKEN"
    verdict=$?
    [ "$verdict" != 0 ] || break
    printf x >>"$POOL_RUN/failures"
    say "$GROUP_WORDS"
    if [ "$verdict" = 1 ]; then nap "$POOL_REFUSE_EVERY"; else nap "$POOL_BACKOFF_MAX"; fi
    [ -z "$STOP" ] || return 0
  done
  say "registering runners in $GROUP_NOTE"
  clean_runners "$TOKEN"
  TOKEN=''
  GROUP_HOLD=''
  local last_group
  last_group=$(now)

  PIDS=()
  WAIT=()
  local slot live reserved pending available next=0 n last_status=0 until_at at
  while [ -z "$STOP" ]; do
    live=0
    reserved=0
    for ((slot = 0; slot < SLOTS; slot++)); do
      if [ -n "${PIDS[$slot]:-}" ]; then
        if kill -0 "${PIDS[$slot]}" 2>/dev/null; then
          live=$((live + 1))
          reserved=$((reserved + SLOT_MEM[slot]))
        else
          wait "${PIDS[$slot]}" 2>/dev/null
          PIDS[slot]=''
          rm -f "$POOL_RUN/slots/$slot"
        fi
      fi
    done
    at=$(now)
    until_at=$(cat "$POOL_RUN/backoff-until" 2>/dev/null)
    case $until_at in '' | *[!0-9]*) until_at=0 ;; esac
    available=$(meminfo_bytes MemAvailable)
    pending=0
    # Admission. A slot starts only while there is a place under the cap and memory for its whole size: memory that
    # no other slot has reserved, and memory the machine has free now. CPU is not reserved. The search starts after
    # the slot admitted last, so every class gets its turn.
    for ((n = 0; n < SLOTS; n++)); do
      slot=$(((next + n) % SLOTS))
      [ -z "${PIDS[$slot]:-}" ] || continue
      if [ -z "$(credential_kind)" ]; then
        WAIT[slot]='waiting for a credential'
      elif [ -n "$GROUP_HOLD" ]; then
        WAIT[slot]='held: its runner group is refused now (see the pool line)'
      elif [ "$until_at" -gt "$at" ]; then
        WAIT[slot]="pausing $((until_at - at))s after a failure"
      elif [ "$live" -ge "$MAX_SLOTS" ]; then
        WAIT[slot]="waiting for a place: $MAX_SLOTS slots is the cap"
      elif [ $((reserved + SLOT_MEM[slot])) -gt "$BUDGET" ]; then
        WAIT[slot]="waiting for $(mib "${SLOT_MEM[$slot]}") MiB of unreserved memory"
      elif [ $((${available:-0} - pending)) -lt "${SLOT_MEM[$slot]}" ]; then
        WAIT[slot]="waiting for $(mib "${SLOT_MEM[$slot]}") MiB of free memory"
      else
        WAIT[slot]=''
        slot_cycle "$slot" &
        PIDS[slot]=$!
        live=$((live + 1))
        reserved=$((reserved + SLOT_MEM[slot]))
        pending=$((pending + SLOT_MEM[slot]))
        next=$(((slot + 1) % SLOTS))
      fi
    done
    # What GitHub says about the runner group (or the repository) can change after the pool has started: it is read
    # again every POOL_GROUP_EVERY seconds. A group that has become one the pool refuses holds every new slot until
    # it is put right; runners that already wait or work are left. A warning shows in the next status block.
    if [ $((at - last_group)) -ge "$POOL_GROUP_EVERY" ]; then
      last_group=$at
      if github_token; then
        check_runner_group "$TOKEN"
        verdict=$?
        TOKEN=''
        case $verdict in
          0) GROUP_HOLD='' ;;
          1)
            [ -n "$GROUP_HOLD" ] || printf x >>"$POOL_RUN/failures"
            GROUP_HOLD=1
            say "$GROUP_WORDS"
            ;;
          *) say "$GROUP_WORDS; the pool goes on as it was and asks again in $POOL_GROUP_EVERY seconds" ;;
        esac
      else
        say "GitHub sign-in failed: $TOKEN_ERROR; the runner group is read again in $POOL_GROUP_EVERY seconds"
      fi
    fi
    if [ $((at - last_status)) -ge "$POOL_STATUS_EVERY" ]; then
      rotate_jobs
      print_status
      last_status=$at
    fi
    nap "$POOL_TICK"
  done

  say "stopping: runners without a job are taken off GitHub, and a job that is running is lost"
  for slot in "${!PIDS[@]}"; do
    [ -z "${PIDS[$slot]}" ] || kill -TERM "${PIDS[$slot]}" 2>/dev/null
  done
  local deadline=$(($(now) + POOL_STOP_WAIT))
  for slot in "${!PIDS[@]}"; do
    [ -n "${PIDS[$slot]}" ] || continue
    while kill -0 "${PIDS[$slot]}" 2>/dev/null && [ "$(now)" -lt "$deadline" ]; do nap 0.2; done
  done
  say "stopped"
}

case ${1-} in
  '') install_pool ;;
  run) run_pool ;;
  status) cat "$POOL_LOGS/status.txt" 2>/dev/null || say "no status yet" ;;
  *)
    echo "usage: arca-runner-pool [run|status]" >&2
    exit 64
    ;;
esac
