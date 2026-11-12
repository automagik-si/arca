#!/bin/bash
# The github-runner-pool service (templates/github-runner-pool.json): one virtual machine that runs many fresh GitHub
# Actions runners. This one file is both halves of it.
#
# - Run with no argument it is the machine's first-boot script: cloud-init runs it once, as root. It installs Docker and
#   the few tools the controller uses from the distribution's own packages, copies itself to
#   /usr/local/sbin/arca-runner-pool and starts the controller as a systemd service. What it says goes to the
#   machine's console too (service_logs); the package manager's own output stays in /var/log/cloud-init-output.log.
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
: "${POOL_CONSOLE:=/dev/console}"
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

# Every program this script starts that bash does not have built in. The first boot refuses to go on while one of
# them is missing, and names it. crates/arca/tests/runner_pool.rs runs the controller with nothing else on its PATH,
# so a program used below and not listed here fails that test.
POOL_PROGRAMS='docker jq curl openssl flock systemctl apt-get hostname awk sed grep find cat chmod chown cut date du
fold head id install mkdir mount mv od rm sha256sum sleep stat tail tee touch tr umount'

# The Debian packages the first boot installs. Recommended packages are not installed, so every package that is needed
# is named here. docker.io is the daemon alone: on Debian 13 the `docker` command is a package of its own, docker-cli,
# which docker.io only recommends. mount and umount, which layer the runner's files, are the package mount, not
# util-linux. The ones the image already carries are named too, so the list says what the script
# stands on.
POOL_PACKAGES='docker.io docker-cli curl jq openssl ca-certificates util-linux mount coreutils hostname'
# The runner image's own user and docker group (the image fixes both).
RUNNER_UID=1001
DOCKER_GID=123

say() { printf 'arca-runner-pool: %s\n' "$*"; }
now() { date +%s; }
# The stop is a line that is read, not a flag that a trap sets. The process that takes the signal (run_pool) holds the
# line open through one program and ends that program when it is told to stop; from then on whoever reads the line
# finds its end. So `stopping` is true from that moment, in the controller and in every slot, and a pause, which is a
# read of the line with a limit of time, ends at once, wherever the signal landed: one that comes while a command runs
# is found when the pause after it is begun.
#
# Neither the controller nor a slot has a trap, because bash 5.2 can lose the signal a trap with a command in it was to
# take. When the signal comes just before bash looks for the end of a $( ) in a word, bash runs the trap there and
# reads the trap's own text as if it stood inside that $( ): the text does not parse, its command is not run, and the
# script runs on as if nothing had been sent, or ends at once without its stop. Both are full of $( ).
STOP_LINE=''
stopping() { read -r -t 0 -u "$STOP_LINE"; }
nap() {
  read -r -t "$1" -u "$STOP_LINE" _
  return 0
}

# ---------------------------------------------------------------------------------------------------------------------
# First boot
# ---------------------------------------------------------------------------------------------------------------------

# cloud-init keeps what the first boot prints in /var/log/cloud-init-output.log, inside the machine. The machine's
# console is what service_logs returns, so the first boot says its own lines there as well: a first boot that fails
# must not leave a machine that is running and silent.
boot_say() {
  say "$*"
  { say "$*" >>"$POOL_CONSOLE"; } 2>/dev/null
  return 0
}

# The programs of POOL_PROGRAMS that are not on this machine, in one line.
missing_programs() {
  local program missing=''
  for program in $POOL_PROGRAMS; do
    command -v "$program" >/dev/null 2>&1 || missing="$missing $program"
  done
  printf '%s' "${missing# }"
}

install_pool() {
  export DEBIAN_FRONTEND=noninteractive
  local try missing
  boot_say "first boot: installing $POOL_PACKAGES"
  for try in 1 2 3 4 5 6; do
    # shellcheck disable=SC2086
    if apt-get update -q && apt-get install -y -q --no-install-recommends $POOL_PACKAGES; then
      break
    fi
    boot_say "first boot: the packages could not be installed (try $try of 6); trying again in 20 seconds"
    sleep 20
  done
  missing=$(missing_programs)
  if [ -n "$missing" ]; then
    boot_say "first boot failed: these commands are missing after the install: $missing. The packages asked for were: $POOL_PACKAGES. The package manager's own words are in /var/log/cloud-init-output.log inside the machine. The machine has no pool; service_remove it and create it again once it can reach its package mirror"
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
  if ! systemctl restart docker || ! systemctl enable --now arca-runner-pool.service; then
    boot_say "first boot failed: Docker or the controller's service did not start. Inside the machine, 'systemctl status docker arca-runner-pool' says why. The machine has no pool"
    exit 1
  fi
  boot_say "first boot done: the controller is installed and started"
}

# ---------------------------------------------------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------------------------------------------------

# The bytes of a size such as 4GiB or 500MB, as Arca's templates write sizes. Prints nothing for anything else.
size_bytes() {
  local value=$1 number unit
  number=${value%%[!0-9]*}
  unit=${value#"$number"}
  # One to six digits, and not zero: six digits of the largest unit still fit the shell's arithmetic, a longer number
  # would wrap around to a negative size, and a slot of no memory is no slot.
  case $number in '' | ???????*) return 1 ;; esac
  number=$((10#$number))
  [ "$number" -gt 0 ] || return 1
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
SLOT_CLASS_N=()
SLOT_LABELS=()
SLOT_LABELS_JSON=()
SLOT_CPU=()
SLOT_MEM=()
CLASS_NAMES=()
# The first slot of each class: every slot of a class has the same size.
CLASS_FIRST=()

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
  if [ ${#GROUP} -gt 15 ] || [ -z "$MAX_SLOTS" ] || [ ${#MAX_SLOTS} -gt 4 ]; then
    say "settings: runnerGroupId and maxSlots are whole numbers"
    return 1
  fi
  MAX_SLOTS=$((10#$MAX_SLOTS))
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
    # Each number is checked for its length before it is compared: a number too long for the shell's arithmetic is
    # an error there, not a size.
    if [ -z "$name" ] || [ -n "${name//[a-z0-9]/}" ] || [ ${#name} -gt 16 ] ||
      [ -z "$labels" ] || [ -n "${labels//[A-Za-z0-9._,-]/}" ] ||
      [ -z "$cpu" ] || [ -n "${cpu//[0-9]/}" ] || [ ${#cpu} -gt 4 ] || [ "$((10#$cpu))" -lt 1 ] ||
      [ -z "$slots" ] || [ -n "${slots//[0-9]/}" ] || [ ${#slots} -gt 3 ] || [ "$((10#$slots))" -lt 1 ] || [ "$((10#$slots))" -gt 256 ] ||
      [ -z "$bytes" ]; then
      say "settings: class ${name:-(unnamed)} needs a name of a-z and 0-9, labels of letters, digits, dots, dashes and underscores, a cpu count, a memory size such as 4GiB and a slots count of 1 to 256"
      return 1
    fi
    cpu=$((10#$cpu))
    slots=$((10#$slots))
    CLASS_FIRST+=("${#SLOT_CLASS[@]}")
    CLASS_NAMES+=("$name")
    for ((n = 0; n < slots; n++)); do
      SLOT_CLASS+=("$name")
      SLOT_CLASS_N+=($((${#CLASS_NAMES[@]} - 1)))
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
  CLASSES=${#CLASS_NAMES[@]}
  # Every class is kept one place under the cap (see "A place for every class" in the controller), so a cap below the
  # number of classes is refused: with fewer places than classes one class would have none, and its jobs would wait
  # for ever beside a pool that looks well. A pool of one class is not asked: its cap is one at least, as before.
  if [ "$CLASSES" -gt 1 ] && [ "$MAX_SLOTS" -lt "$CLASSES" ]; then
    say "settings: maxSlots is $MAX_SLOTS and classes names $CLASSES classes. Every class is kept one place under the cap, so maxSlots is never below the number of classes: with $MAX_SLOTS places, $((CLASSES - MAX_SLOTS)) of the $CLASSES classes would have no runner and their jobs would wait for ever. Set maxSlots to $CLASSES or more, or name fewer classes"
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
  # API_SENT is how many bytes of the request went out. With no answer (000) it tells a request GitHub never had (a
  # refused connection, curl's exit 7: 0) from one whose answer was lost (the connection closed after it, exit 52, or
  # the time ran out, exit 28: the whole request).
  API_SENT=0
  case $token in *[!A-Za-z0-9._~+/=-]* | '')
    API_CODE=000
    API_ERROR='the credential has a character no GitHub token has'
    return 1
    ;;
  esac
  local written args=(-sS -X "$method" -o "$API_OUT" -w '%{http_code} %{size_request}' --max-time 30 --connect-timeout 10 -K -)
  [ -n "$body" ] && args+=(--data-binary "$body" -H 'Content-Type: application/json')
  written=$(printf 'header = "Authorization: Bearer %s"\nheader = "Accept: application/vnd.github+json"\nheader = "X-GitHub-Api-Version: 2022-11-28"\nheader = "User-Agent: arca-runner-pool"\n' "$token" |
    curl "${args[@]}" "$API_URL$path" 2>/dev/null) || written="000 ${written#* }"
  API_CODE=${written%% *}
  API_SENT=${written##* }
  case $API_CODE in [0-9][0-9][0-9]) ;; *) API_CODE=000 ;; esac
  case $API_SENT in '' | *[!0-9]*) API_SENT=0 ;; esac
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

# The name of the job a runner's output names: what follows "Running job: " on the runner's own line, which begins
# with the time. A line of another shape is read as before.
job_name() {
  local name
  name=$(printf '%s\n' "$1" | sed -n 's/^[0-9]\{4\}-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9]Z: Running job: //p' | head -n1)
  [ -n "$name" ] || name=$(printf '%s\n' "$1" | sed -n 's/.*Running job: //p' | head -n1)
  plain "$name"
}

# When that job began, as seconds: the runner begins the line with the time. Only the time at the very start of the
# line is read, so a job whose name holds a time and the words "Running job:" cannot set it. Now, when the time
# cannot be read.
job_began() {
  local stamp at
  stamp=$(printf '%s\n' "$1" | sed -n 's/^\([0-9]\{4\}-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9]\)Z: Running job: .*/\1/p' | head -n1)
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
  local retired='' asked=''
  # A slot takes no signal. It reads the same line as the controller: a slot begun as the controller is told to stop
  # finds the stop at once.
  trap - TERM INT
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
    if stopping || [ "$tries" -gt "$POOL_READY_WAIT" ]; then
      slot_abandon "$slot" "$runner_id"
      stopping || slot_failed "$slot" "the slot's Docker daemon did not come up within $POOL_READY_WAIT seconds"
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

  while ! stopping; do
    state=$(docker inspect -f '{{.State.Running}} {{.State.ExitCode}}' "$runner" 2>/dev/null) || state='false 255'
    [ "${state%% *}" = true ] || break
    # A runner that has taken its job says so in its own output, with the job's name and the time: "Running job:".
    # That line is what marks the slot busy. (The slot was once asked for the runner's processes instead, in a form
    # Docker refuses, so a running job was shown as idle.) The output is read only until the job has begun.
    lines=''
    [ -n "$busy_since" ] || lines=$(docker logs "$runner" 2>&1)
    case $lines in *'Running job: '*)
      busy_since=$(job_began "$lines")
      job=$(job_name "$lines")
      set_state "$slot" busy "$name ${job:+job \"$job\"}"
      ;;
    esac
    # GitHub tells a runner older than its newest release to update itself before it takes a job. It does so inside
    # its own container; the pool says it once per runner, because every fresh runner will until the pin moves.
    if [ -z "$updating" ] && [ -z "$busy_since" ] && [[ $lines == *'Runner update in progress'* ]]; then
      updating=1
      # GitHub still has this runner: a request to take it off made before now did not.
      asked=''
      printf x >>"$POOL_RUN/updates"
      set_state "$slot" updating "$name"
      say "slot $slot: GitHub told this runner to update itself, because the pool's pinned runner is older than GitHub's newest. It updates inside its own container and then takes its job; every new runner of this pool does the same, at the cost of a download each, until a pool is created with a newer runnerImage"
    fi
    # The runner's own wrapper says when it starts the updated runner: from then on the update is over.
    if [ "$updating" = 1 ] && [[ $lines == *'Restarting runner'* ]]; then
      updating=finished
      [ -n "$busy_since" ] || set_state "$slot" idle "$name"
    fi
    # The controller asks a runner that waits for a job to give its place to a class that has none.
    if [ -e "$POOL_RUN/retire/$slot" ] && slot_retire "$slot" "$runner_id" "$runner" "$busy_since" "$updating"; then
      retired=1
      break
    fi
    nap "$POOL_POLL"
  done

  if [ -n "$retired" ]; then
    # GitHub has taken the runner off, or no longer had it: it took no job and is cleaned up as one that ended.
    slot_reap "$slot"
    return 0
  fi

  if stopping; then
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
    job=$(job_name "$lines")
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
    if [ -n "$asked" ]; then
      # GitHub was asked to take this runner off and can have done so though no yes came: it answered with a server
      # error, or its answer was lost (slot_retire). The runner then stops like this. That is no failure of the pool:
      # nothing is counted, no slot pauses, and the runners keep their layered files. Why it stopped is not known:
      # this is the likely cause, not a seen one. A runner GitHub has told to update itself since the request is not
      # read so ($asked is cleared then): that word is proof GitHub still had it, and its stop is the failure it would
      # be for any runner. One that had updated before it was asked is read so like any other.
      say "slot $slot ($class): its runner stopped (exit $code) before it took a job, after GitHub was asked to take it off and gave no clear answer. It is taken as retired: its place is free, and it is not counted as a failure"
      return 0
    fi
    case $updating in
      1)
        slot_failed "$slot" "its runner stopped (exit $code) while it was updating itself, before it took a job"
        # The update replaces the runner's own files: if it fails with them layered, the next runners start plain.
        [ -z "$layered" ] || plain_home "A runner stopped while it was updating itself with its files layered"
        ;;
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

# Gives up the place of a runner that waits for a job, when the controller asks for it (give_a_place). That a runner
# is idle is read at one moment and its container is removed at another, and GitHub can hand it a job in between. So
# GitHub is asked first to take the registration off, and the container is removed only on its yes. What the real
# service was seen to answer, at an organisation:
# - 422 for a runner that is running a job. It is kept.
# - 204 for every other runner: one that waits for a job, and also one GitHub no longer has or never had. So its yes
#   is not yet a retirement: a just-in-time runner that took a job and ended it while this was asked has been removed
#   by GitHub itself, and the answer is the same 204. The runner's own output tells the two apart, and it is read
#   after the answer: a runner that has taken a job is left to end, so its job is recorded as every job is.
# After a yes for a runner that has taken no job, the runner gets none, and only then is its place given up.
# Every other answer is no yes, a 404 too: GitHub does not answer 404 for a runner that is gone, so a 404 says that
# the request did not reach the runner (a credential that no longer sees it, the address), and GitHub may still hold
# the registration and hand it a job. No answer's text is read, only its status.
# What came of it is written for the controller:
# - `retired`: GitHub took the runner off, or no longer had it, and it took no job. Returns 0, and the slot ends.
# - `job`: the runner has a job, or has had its one job: the slot has seen it take one, GitHub says so, or its output
#   shows one after GitHub's yes. It is kept and is not asked again.
# - `updating`: GitHub is not asked: the runner is replacing its own files (GitHub tells a runner to update itself
#   before it takes a job). It is kept, and may be asked again once its update is over.
# - `kept`: GitHub is not asked, and the runner is not asked again: GitHub gave no id for it, and a request without
#   one names no runner.
# - `error` and words: GitHub could not be asked, or answered something else. The runner is kept and may be asked
#   again. $asked is set for the slot when GitHub can have acted on the request all the same: it answered with a
#   server error (5xx), or the request went out and no answer came (the connection closed after it, or the time ran
#   out). It is not set when GitHub said it did not act (401, 403, 404), nor when the request never went out (a
#   refused connection, a credential with a character no token has).
slot_retire() {
  local slot=$1 runner_id=$2 runner=$3 busy=$4 updating=$5 class=${SLOT_CLASS[$1]} for_class answer
  for_class=$(cat "$POOL_RUN/retire/$slot" 2>/dev/null)
  if [ -n "$busy" ]; then
    answer=job
  elif [ "$updating" = 1 ]; then
    answer=updating
  elif [ -z "$runner_id" ] || [ -n "${runner_id//[0-9]/}" ]; then
    answer=kept
  elif ! github_token; then
    answer="error GitHub sign-in failed: $TOKEN_ERROR"
  elif api DELETE "$SCOPE/actions/runners/$runner_id" "$TOKEN"; then
    if [[ $(docker logs "$runner" 2>&1) == *'Running job: '* ]]; then
      answer=job
    else
      answer=retired
      say "slot $slot ($class): its idle runner is retired and its place goes to class $for_class, which had no runner while every place under the cap of $MAX_SLOTS was held"
    fi
  elif [ "$API_CODE" = 422 ]; then
    answer=job
    say "slot $slot ($class): its runner is kept: it was asked to give its place to class $for_class, and GitHub says it has a job ($API_ERROR)"
  else
    # A token GitHub no longer takes is not kept.
    [ "$API_CODE" != 401 ] || rm -f "$POOL_RUN/token"
    case $API_CODE in
      5??) asked=1 ;;
      000) [ "$API_SENT" = 0 ] || asked=1 ;;
    esac
    answer="error $API_ERROR"
  fi
  TOKEN=''
  rm -f "$API_OUT"
  printf '%s\n' "$answer" >"$POOL_RUN/retire/$slot.answer.new" &&
    mv "$POOL_RUN/retire/$slot.answer.new" "$POOL_RUN/retire/$slot.answer"
  rm -f "$POOL_RUN/retire/$slot"
  [ "$answer" != retired ] || return 0
  return 1
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
      ! stopping || return 1
      say "the image $image could not be pulled; trying again in 30 seconds"
      nap 30
      ! stopping || return 1
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
    [ -z "$NEVER_NOTE" ] || say "$NEVER_NOTE"
    [ -z "$FIT_NOTE" ] || say "$FIT_NOTE"
    [ -z "$CAP_NOTE" ] || say "$CAP_NOTE"
    [ -z "$CLASS_NOTE" ] || say "$CLASS_NOTE"
    say "totals busy=$busy idle=$idle starting=$starting queued=$queued jobs_done=$(count_of jobs) failures=$(count_of failures) runner_updates=$(count_of updates) memory_reserved=$(mib "$reserved")MiB of $(mib "$BUDGET")MiB slots_cap=$MAX_SLOTS"
  } | tee "$POOL_LOGS/status.txt.new"
  mv "$POOL_LOGS/status.txt.new" "$POOL_LOGS/status.txt"
}

# A place for every class. The cap (maxSlots) counts registered runners, and a runner that is registered and waits for
# a job keeps its place until a job ends somewhere. Without more, a class whose slot could not start when the places
# were handed out (its memory was not free then) would wait behind the idle runners of the other classes, with its jobs.
# So, under the cap, every class is kept one place, in three parts:
# - the cap is never below the number of classes (load_settings refuses it);
# - a free place goes to the class that has the fewest runners (round_begin and next_slot);
# - when a class has no runner and every place is held, one runner that waits for a job gives its place up
#   (give_a_place, and slot_retire in the slot).
# What this is not: a cap on running jobs. A just-in-time runner cannot be told to wait, so every registered runner may
# take a job at any moment, and a runner that has one is never retired: a class whose place is held by runners at work
# waits until one ends. Nor is a place memory: a slot still starts only when its whole size fits.
# These functions read and change the controller's own variables (pool's): bash lets a function it calls do so.

# The pass that hands out places looks at every slot without a runner once. round_begin lists them for each class in
# the order of the round, which starts at $first; next_slot then gives the one to look at next, in $slot: of the slots
# not yet looked at, one of the class with the fewest runners; among several such classes, and among the slots of one
# class, the first in the round. It returns 1 when every slot has been looked at. With one class this is the round
# alone, as it was before classes were counted. The slots are walked once for a pass, however many they are.
round_begin() {
  local m s c
  for ((c = 0; c < CLASSES; c++)); do round[c]=''; done
  for ((m = 0; m < SLOTS; m++)); do
    s=$(((first + m) % SLOTS))
    [ -z "${PIDS[$s]:-}" ] || continue
    # The slot, and where it stands in the round.
    round[SLOT_CLASS_N[s]]+="$s:$m "
  done
}

next_slot() {
  local c head from='' least='' place=''
  for ((c = 0; c < CLASSES; c++)); do
    [ -n "${round[$c]}" ] || continue
    head=${round[$c]%% *}
    if [ -z "$from" ] || [ "${class_live[$c]}" -lt "$least" ] ||
      { [ "${class_live[$c]}" = "$least" ] && [ "${head#*:}" -lt "$place" ]; }; then
      from=$c
      least=${class_live[$c]}
      place=${head#*:}
    fi
  done
  [ -n "$from" ] || return 1
  head=${round[$from]%% *}
  slot=${head%:*}
  round[from]=${round[$from]#* }
}

# When a class has no runner and every place under the cap is held: asks one idle runner to give its place up, and
# says in $CLASS_NOTE, for the status block, which class waits and for what. One runner at a time. It is a runner of
# the class that holds the most places among the classes that hold two or more and have an idle runner: never a
# class's only runner, or the two classes would trade the place at every pass. Nothing is asked while the place is not
# the one thing the class lacks: while no runner can be registered at all, or when the class's slot would still not
# have its memory with the idle runner gone. A runner would then be retired at every pass, for nothing. After an
# answer of GitHub's that was neither yes nor no, the next runner is asked after a pause.
give_a_place() {
  local c s phase rest answer names='' waiting=() want='' give='' most=1 need error='' last=''
  if [ -n "$RETIRING" ]; then
    if [ -z "${PIDS[$RETIRING]:-}" ]; then
      # It has ended: retired, or by itself.
      RETIRING=''
      RETIRE_ERROR=''
      RETIRE_PAUSE=0
    elif answer=$(cat "$POOL_RUN/retire/$RETIRING.answer" 2>/dev/null) && [ -n "$answer" ]; then
      case $answer in
        job | kept)
          rm -f "$POOL_RUN/retire/$RETIRING.answer"
          KEPT[RETIRING]=1
          RETIRING=''
          RETIRE_ERROR=''
          RETIRE_PAUSE=0
          ;;
        updating)
          # It is asked again once its update is over: until then its phase keeps it from being chosen.
          rm -f "$POOL_RUN/retire/$RETIRING.answer"
          RETIRING=''
          RETIRE_ERROR=''
          RETIRE_PAUSE=0
          ;;
        'error '*)
          # The next runner is asked after a pause that grows as the pause after a failed registration does: five
          # seconds, then twice as long each time, up to POOL_BACKOFF_MAX. It is this request's own pause: no slot
          # waits for it.
          rm -f "$POOL_RUN/retire/$RETIRING.answer"
          error=${answer#error }
          RETIRING=''
          RETIRE_PAUSE=$((RETIRE_PAUSE == 0 ? 5 : RETIRE_PAUSE * 2))
          [ "$RETIRE_PAUSE" -le "$POOL_BACKOFF_MAX" ] || RETIRE_PAUSE=$POOL_BACKOFF_MAX
          RETIRE_AFTER=$((at + RETIRE_PAUSE))
          ;;
        # Retired: the slot is cleaning up, and its place is free when it has ended.
        *)
          RETIRE_ERROR=''
          RETIRE_PAUSE=0
          ;;
      esac
    fi
  fi
  CLASS_NOTE=''
  if [ "$CLASSES" -gt 1 ] && [ "$live" -ge "$MAX_SLOTS" ]; then
    for ((c = 0; c < CLASSES; c++)); do
      [ "${class_live[$c]}" = 0 ] || continue
      waiting+=("$c")
      names="${names:+$names, }${CLASS_NAMES[$c]}"
    done
  fi
  if [ ${#waiting[@]} = 0 ]; then
    RETIRE_ERROR=''
    RETIRE_PAUSE=0
    return 0
  fi
  names="class $names: no runner, and every place under the cap of $MAX_SLOTS is held"
  if [ -n "$error" ] && [ "$error" != "$RETIRE_ERROR" ]; then
    # Said once, and kept for the status block: the next pass asks again.
    RETIRE_ERROR=$error
    say "$names. An idle runner was asked to give its place up and is kept, because GitHub did not take it off: $error. One is asked again after a pause, of $RETIRE_PAUSE seconds now"
  fi
  [ -z "$RETIRE_ERROR" ] || last=". The last time one was asked, GitHub did not take the runner off: $RETIRE_ERROR"
  if [ -n "$RETIRING" ]; then
    CLASS_NOTE="$names. Slot $RETIRING (${SLOT_CLASS[$RETIRING]}) gives up its idle runner's place for it$last"
    return 0
  fi
  if [ -z "$(credential_kind)" ] || [ -n "$GROUP_HOLD" ] || [ "$until_at" -gt "$at" ]; then
    CLASS_NOTE="$names. No idle runner is retired for it while no new runner can be registered (the slots' own lines say why)"
    return 0
  fi
  if [ "$RETIRE_PAUSE" != 0 ] && [ "$at" -lt "$RETIRE_AFTER" ]; then
    CLASS_NOTE="$names. An idle runner is asked again in $((RETIRE_AFTER - at))s$last"
    return 0
  fi
  for ((c = 0; c < CLASSES; c++)); do
    [ "${class_live[$c]}" -gt "$most" ] || continue
    for ((s = 0; s < SLOTS; s++)); do
      [ "${SLOT_CLASS_N[$s]}" = "$c" ] && [ -n "${PIDS[$s]:-}" ] && [ -z "${KEPT[$s]:-}" ] || continue
      { IFS=$'\t' read -r phase rest <"$POOL_RUN/slots/$s"; } 2>/dev/null || phase=''
      [ "$phase" = idle ] || continue
      give=$s
      most=${class_live[$c]}
      break
    done
  done
  if [ -z "$give" ]; then
    CLASS_NOTE="WARNING: $names, and no class that holds two or more has a runner that can be asked to give its place up. A runner that has a job, that is starting, or that GitHub gave no id for, is never retired, and one that is updating itself not before its update is over: the class waits until a job ends or an update is over"
    return 0
  fi
  # The class the place is asked for: the first of those without a runner whose slot would have its memory.
  for c in "${waiting[@]}"; do
    need=${SLOT_MEM[${CLASS_FIRST[$c]}]}
    if [ $((reserved - SLOT_MEM[give] + need)) -le "$BUDGET" ] && [ "${available:-0}" -ge "$need" ]; then
      want=$c
      break
    fi
  done
  if [ -z "$want" ]; then
    c=${waiting[0]}
    CLASS_NOTE="WARNING: $names. No idle runner is retired for class ${CLASS_NAMES[$c]}: its slot of $(mib "${SLOT_MEM[${CLASS_FIRST[$c]}]}") MiB would still not have its memory. A place is not memory"
    return 0
  fi
  printf '%s\n' "${CLASS_NAMES[$want]}" >"$POOL_RUN/retire/$give.new" && mv "$POOL_RUN/retire/$give.new" "$POOL_RUN/retire/$give"
  RETIRING=$give
  CLASS_NOTE="$names. Slot $give (${SLOT_CLASS[$give]}) gives up its idle runner's place for it$last"
}

# The controller: everything the pool does, as a child of run_pool. It takes no signal. It learns of a stop from the
# line, and then stops in its own order. A SIGTERM sent to this process itself ends it at once with no stop; nothing
# sends one under the unit's KillMode=mixed. It does not ignore the signal instead, because everything it starts,
# its slots first, would then ignore it too, and nothing of the pool could be ended with that signal any more.
pool() {
  trap - TERM INT
  load_settings || {
    say "the pool does not start until its settings are right: they were fixed when the machine was created, so create the pool again with the right ones"
    # Stay up, so the reason stays on the console instead of a restart loop.
    while ! stopping; do nap 300; done
    return 1
  }
  mkdir -p "$POOL_STATE/slots" "$POOL_LOGS" "$POOL_RUN/slots" "$POOL_RUN/tmp" "$POOL_RUN/jit" "$POOL_RUN/retire"
  chmod 700 "$POOL_RUN" "$POOL_RUN/tmp" "$POOL_RUN/jit"
  rm -f "${POOL_RUN:?}"/slots/* "${POOL_RUN:?}"/jit/* "${POOL_RUN:?}/jobs" "${POOL_RUN:?}/failures" "${POOL_RUN:?}/updates" "${POOL_RUN:?}/backoff" "${POOL_RUN:?}/backoff-until" "${POOL_RUN:?}/plain-home" "${POOL_RUN:?}"/tmp/* "${POOL_RUN:?}"/retire/*
  load_pool_id
  JOBS_HEADER=$(printf 'finished\tslot\tclass\tlabels\tjob\tresult\tseconds\tpeak_memory_bytes\tcpu_seconds\trunner')
  [ -s "$POOL_LOGS/jobs.tsv" ] || printf '%s\n' "$JOBS_HEADER" >"$POOL_LOGS/jobs.tsv"
  local total wanted=0 slot
  total=$(meminfo_bytes MemTotal)
  BUDGET=$((${total:-0} - RESERVE))
  GROUP_NOTE=''
  GROUP_WORDS=''
  say "pool $POOL_NAME id $POOL_ID for $URL: $SLOTS slots in ${#CLASS_NAMES[@]} classes, at most $MAX_SLOTS at once, $(mib "$BUDGET") MiB of memory for slots"
  # Slots that the cap lets run all at once need their memory all at once. The machine reports less memory than the
  # size it was given, so sizes that add up to the machine's size do not fit, and one slot would stay queued for ever
  # beside idle ones. A slot that is larger than all the memory there is for slots never starts, whatever the cap.
  # Both are said at the start and in every status block, with the numbers.
  FIT_NOTE=''
  NEVER_NOTE=''
  local never=''
  for slot in "${!SLOT_MEM[@]}"; do
    wanted=$((wanted + SLOT_MEM[slot]))
    [ "${SLOT_MEM[$slot]}" -gt "$BUDGET" ] || continue
    case ", $never" in
      *", ${SLOT_CLASS[$slot]} ("*) ;;
      *) never="${never:+$never, }${SLOT_CLASS[$slot]} ($(mib "${SLOT_MEM[$slot]}") MiB)" ;;
    esac
  done
  if [ -n "$never" ]; then
    NEVER_NOTE="WARNING: a slot of these classes is larger than the $(mib "$BUDGET") MiB the machine has for slots (the $(mib "${total:-0}") MiB it reports, less reserveMemory $(mib "$RESERVE") MiB): $never. Such a slot never starts, and the jobs that ask for its labels wait for ever. Make the class or reserveMemory smaller, or the machine larger, in the template, and create the pool again"
    say "$NEVER_NOTE"
  fi
  if [ "$SLOTS" -le "$MAX_SLOTS" ] && [ "$wanted" -gt "$BUDGET" ]; then
    FIT_NOTE="WARNING: the $SLOTS slots ask for $(mib "$wanted") MiB together and the machine has $(mib "$BUDGET") MiB for slots (the $(mib "${total:-0}") MiB it reports, less reserveMemory $(mib "$RESERVE") MiB): $(mib $((wanted - BUDGET))) MiB too much, so they never all run at once. A machine reports less memory than the size it was given. Make the slots or reserveMemory smaller, or the machine larger, in the template, and create the pool again"
    say "$FIT_NOTE"
  fi
  # What the cap is and is not, where it can hold a class back: said at the start and in every status block.
  CAP_NOTE=''
  CLASS_NOTE=''
  if [ "$CLASSES" -gt 1 ] && [ "$SLOTS" -gt "$MAX_SLOTS" ]; then
    CAP_NOTE="every class is kept one place under the cap of $MAX_SLOTS: a free place goes to the class with the fewest runners, and when a class has none and every place is held, one runner that waits for a job is retired for it, of the class that holds the most. The cap counts registered runners, not running jobs: a registered runner cannot be told to wait and one that has a job is never retired, so a class can wait until a job ends; and a place is not memory"
    say "$CAP_NOTE"
  fi
  write_slices

  local waited=0
  while ! stopping && [ -z "$(credential_kind)" ]; do
    if [ $((waited % 60)) = 0 ]; then
      say "waiting for a credential: set GITHUB_APP_CLIENT_ID and GITHUB_APP_PRIVATE_KEY, or GITHUB_TOKEN, with service_secret_set, then run instance_start"
    fi
    nap 5
    waited=$((waited + 5))
  done
  ! stopping || return 0
  if [ "$(credential_kind)" = app ] && [ -s "$(secret_file GITHUB_TOKEN)" ]; then
    say "signing in as the GitHub App: GITHUB_TOKEN is set too and is not used"
  fi
  until prepare_images; do
    ! stopping || return 0
    nap 30
    ! stopping || return 0
  done
  # A stop is read after every wait and every pause below, before anything else is begun: GitHub is not asked again.
  ! stopping || return 0
  clean_containers
  # No slot starts before GitHub takes the credential: a wrong one is said here, once per pause, not once per slot.
  local pause=0
  until github_token; do
    ! stopping || return 0
    printf x >>"$POOL_RUN/failures"
    pause=$((pause == 0 ? 5 : pause * 2))
    [ "$pause" -gt "$POOL_BACKOFF_MAX" ] && pause=$POOL_BACKOFF_MAX
    say "GitHub sign-in failed: $TOKEN_ERROR. Trying again in $pause seconds"
    nap "$pause"
    ! stopping || return 0
  done
  ! stopping || return 0
  # Nor before its runner group is one a pool may use. A refusal is told again every POOL_REFUSE_EVERY seconds, so it
  # stays on the console; only a pool created with other settings ends it.
  local verdict
  while :; do
    github_token || {
      ! stopping || return 0
      printf x >>"$POOL_RUN/failures"
      say "GitHub sign-in failed: $TOKEN_ERROR. Trying again in $POOL_BACKOFF_MAX seconds"
      nap "$POOL_BACKOFF_MAX"
      ! stopping || return 0
      continue
    }
    check_runner_group "$TOKEN"
    verdict=$?
    ! stopping || return 0
    [ "$verdict" != 0 ] || break
    printf x >>"$POOL_RUN/failures"
    say "$GROUP_WORDS"
    if [ "$verdict" = 1 ]; then nap "$POOL_REFUSE_EVERY"; else nap "$POOL_BACKOFF_MAX"; fi
    ! stopping || return 0
  done
  say "registering runners in $GROUP_NOTE"
  clean_runners "$TOKEN"
  TOKEN=''
  GROUP_HOLD=''
  local last_group
  last_group=$(now)

  PIDS=()
  WAIT=()
  # The slot whose idle runner is asked to give its place up, when one is; the slots whose runner is not asked again;
  # what GitHub last answered when it did not take a runner off, and the pause before the next one is asked.
  RETIRING=''
  KEPT=()
  RETIRE_ERROR=''
  RETIRE_PAUSE=0
  RETIRE_AFTER=0
  local slot live reserved pending available next=0 first n last_status=0 until_at at class_live=() round=()
  while ! stopping; do
    live=0
    reserved=0
    for ((n = 0; n < CLASSES; n++)); do class_live[n]=0; done
    for ((slot = 0; slot < SLOTS; slot++)); do
      if [ -n "${PIDS[$slot]:-}" ]; then
        if kill -0 "${PIDS[$slot]}" 2>/dev/null; then
          live=$((live + 1))
          reserved=$((reserved + SLOT_MEM[slot]))
          class_live[SLOT_CLASS_N[slot]]=$((class_live[SLOT_CLASS_N[slot]] + 1))
        else
          wait "${PIDS[$slot]}" 2>/dev/null
          PIDS[slot]=''
          KEPT[slot]=''
          rm -f "$POOL_RUN/slots/$slot" "$POOL_RUN/retire/$slot" "$POOL_RUN/retire/$slot.answer"
        fi
      fi
    done
    at=$(now)
    until_at=$(cat "$POOL_RUN/backoff-until" 2>/dev/null)
    case $until_at in '' | *[!0-9]*) until_at=0 ;; esac
    available=$(meminfo_bytes MemAvailable)
    pending=0
    give_a_place
    # Admission. A slot starts only while there is a place under the cap and memory for its whole size: memory that
    # no other slot has reserved, and memory the machine has free now. CPU is not reserved. Every slot without a
    # runner is looked at once in a pass. A place goes to the class with the fewest runners first, so every class has
    # one before any has two. Among equals the slots go round-robin: the round starts after the slot admitted last
    # and is read once for the pass. (A pass that moved its start while it walked left slots out.) Round-robin is not
    # "the one that waited longest first": a slot that has just ended can come before one that waited a pass longer.
    first=$next
    round_begin
    while next_slot; do
      # No slot is begun once the controller is told to stop.
      ! stopping || break
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
        class_live[SLOT_CLASS_N[slot]]=$((class_live[SLOT_CLASS_N[slot]] + 1))
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

  # The slots have read the same line: each is cleaning up already. They are sent no signal, which would end them
  # in the middle of it.
  say "stopping: runners without a job are taken off GitHub, and a job that is running is lost"
  local deadline=$(($(now) + POOL_STOP_WAIT))
  for slot in "${!PIDS[@]}"; do
    [ -n "${PIDS[$slot]}" ] || continue
    # The controller is stopping already, so this is a plain pause: nap would not begin one.
    while kill -0 "${PIDS[$slot]}" 2>/dev/null && [ "$(now)" -lt "$deadline" ]; do sleep 0.2; done
  done
  say "stopped"
}

# The process systemd stops. From the line that sets its trap on it runs NO command substitution, so its trap is
# always read as it is written: it only starts the controller, waits for it, and lets go of the line when it is told to
# stop. The controller then stops in its own order, and this process ends with the controller's code.
run_pool() {
  local code again ended=''
  exec {STOP_LINE}< <(exec sleep infinity)
  STOP_HOLD=$!
  LET_GO=''
  # Once only: a second signal must not reach whatever has the holder's process number by then.
  trap '[ -n "$LET_GO" ] || { LET_GO=1; kill "$STOP_HOLD" 2>/dev/null; }' TERM INT
  pool &
  POOL=$!
  while [ -z "$ended" ]; do
    wait "$POOL"
    code=$?
    # wait also returns when the trap ran. The controller's code is the one wait gives once the controller is gone;
    # a wait that the trap cut short just as the controller ended is asked again.
    if ! kill -0 "$POOL" 2>/dev/null; then
      wait "$POOL"
      again=$?
      [ "$again" = 127 ] || code=$again
      ended=$code
    fi
  done
  # A controller that ended by itself leaves the line held.
  [ -n "$LET_GO" ] || kill "$STOP_HOLD" 2>/dev/null
  return "$ended"
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
