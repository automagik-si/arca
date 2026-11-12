#!/bin/bash
# The gitlab-runner-pool service (templates/gitlab-runner-pool.json): one virtual machine that runs the CI jobs of one
# GitLab, for several runner registrations at once. This one file is both halves of it.
#
# - Run with no argument it is the machine's first-boot script: cloud-init runs it once, as root. It installs Docker and
#   the few tools the controller uses from the distribution's own packages, copies itself to
#   /usr/local/sbin/arca-gitlab-pool and starts the controller as a systemd service. What it says goes to the machine's
#   console too (service_logs); the package manager's own output stays in /var/log/cloud-init-output.log.
# - `arca-gitlab-pool run` is the controller. It checks the settings against the machine, asks GitLab whether it takes
#   each registration's token, writes GitLab Runner's config.toml and runs GitLab Runner as one container from its
#   image, by digest. GitLab Runner starts every job in a fresh container of the job's own image (the docker executor)
#   and removes it afterwards. The controller watches it, reads the result of every job from its output, and prints a
#   status block.
#
# What isolates jobs: each job is a set of containers (its own, its services', GitLab Runner's helper) on a network
# of its own, under ONE Docker daemon in ONE virtual machine. No job container is privileged, none gets the daemon's
# socket, and nothing a job wrote is kept for the next one. That is container isolation, which a kernel bug or a
# misconfiguration can break: the virtual machine is the boundary that holds. Everything in one pool should trust
# everything else in it as far as two jobs of one GitLab project trust each other.
#
# What it reads:
# - /etc/arca/settings.json: the service's settings (the template's, with what its service_create replaced), written
#   by cloud-init at the first boot and fixed from then on.
# - /etc/arca-secrets/GITLAB_RUNNER_TOKEN_<n>: the runner authentication token (it begins glrt-) of the registration
#   at place <n> of settings.runners, counted from 1. GitLab shows it once, when the runner is created there; its tags
#   and whether it runs untagged jobs are set there too. Written by Arca at each start (service_secret_set, then
#   instance_start).
# - /etc/arca-secrets/TAILSCALE_AUTHKEY: only with settings.reach "tailnet", the key the machine joins the tailnet
#   with.
#
# What it writes: /etc/gitlab-runner/config.toml (mode 0600, in a directory of mode 0700), a status block and one line
# per finished job on the machine's console (service_logs), the same job lines in /var/log/arca-gitlab-pool/jobs.tsv
# (bounded: one older file is kept), and the last status block in /var/log/arca-gitlab-pool/status.txt.
#
# No credential is ever printed or put on a command line. A token reaches curl on its standard input and config.toml
# through the shell's own printf; GitLab's answer to the token check, which repeats the token, is not kept. The
# tailnet node is told the path of its key's file (TS_AUTHKEY=file:<path>), never the key. GitLab Runner's own output
# names a registration by the first characters of its token, as GitLab's pages do; the controller copies none of that
# to the console.
#
# Every path, pause and external program below can be replaced through the environment, which is how the test in
# crates/arca/tests/gitlab_runner_pool.rs drives the controller against a fake GitLab and a fake docker.
set -u

: "${POOL_SETTINGS:=/etc/arca/settings.json}"
: "${POOL_SECRETS:=/etc/arca-secrets}"
: "${POOL_STATE:=/var/lib/arca-gitlab-pool}"
: "${POOL_LOGS:=/var/log/arca-gitlab-pool}"
: "${POOL_RUN:=/run/arca-gitlab-pool}"
: "${POOL_CONFIG:=/etc/gitlab-runner}"
: "${POOL_MEMINFO:=/proc/meminfo}"
: "${POOL_DOCKER_ROOT:=/var/lib/docker}"
: "${POOL_DOCKER_SOCKET:=/var/run/docker.sock}"
: "${POOL_CONSOLE:=/dev/console}"
: "${POOL_TICK:=5}"            # seconds between two passes of the controller
: "${POOL_STATUS_EVERY:=60}"   # seconds between two status blocks
: "${POOL_VERIFY_EVERY:=600}"  # seconds between two askings of GitLab whether it still takes the tokens
: "${POOL_RECHECK:=30}"        # seconds before GitLab is asked again about a changed secret it could not be asked about
: "${POOL_BACKOFF_MAX:=300}"   # the longest pause after failures, in seconds
: "${POOL_STOP_WAIT:=60}"      # seconds the running jobs are given when the controller stops
: "${POOL_REFUSE_EVERY:=300}"  # seconds between two tellings of why the pool does not start
: "${POOL_CONNECT_WAIT:=10}"   # seconds GitLab is given to accept a connection
: "${POOL_HTTP_WAIT:=30}"      # seconds GitLab is given to answer a request
: "${POOL_READY_WAIT:=120}"    # seconds the tailnet node is given to come up
: "${POOL_JOBS_MAX:=4194304}"  # bytes of jobs.tsv before it is set aside as jobs.tsv.1
: "${POOL_STATUS_JOBS:=10}"    # finished jobs repeated in each status block

INSTALLED=/usr/local/sbin/arca-gitlab-pool
# A template holds at most 16 secrets, and a secret's name is fixed in the template: twelve registrations and the
# tailnet key.
MAX_RUNNERS=12
# The control socket of the tailnet node, inside its container.
TAILNET_SOCKET=/tmp/tailscaled.sock

# Every program this script starts that bash does not have built in. The first boot refuses to go on while one of
# them is missing, and names it. crates/arca/tests/gitlab_runner_pool.rs runs the controller with nothing else on its
# PATH, so a program used below and not listed here fails that test.
POOL_PROGRAMS='docker jq curl systemctl apt-get hostname getent timeout nproc df awk sed grep cat chmod cut date
install mkdir mv od rm sha256sum sleep sort stat tail tee tr'

# The Debian packages the first boot installs. Recommended packages are not installed, so every package that is needed
# is named here. docker.io is the daemon alone: on Debian 13 the `docker` command is a package of its own, docker-cli,
# which docker.io only recommends. getent is libc-bin's. The ones the image already carries are named too, so the list
# says what the script stands on.
POOL_PACKAGES='docker.io docker-cli curl jq ca-certificates coreutils libc-bin hostname'

say() { printf 'arca-gitlab-pool: %s\n' "$*"; }
now() { date +%s; }
# The stop is a line that is read, not a flag that a trap sets. The process that takes the signal (run_pool) holds the
# line open through one program and ends that program when it is told to stop; from then on whoever reads the line
# finds its end. So `stopping` is true from that moment, in the controller and in everything it starts, and a pause,
# which is a read of the line with a limit of time, ends at once, wherever the signal landed: one that comes while a
# command runs is found when the pause after it is begun.
#
# The controller itself has no trap, because bash 5.2 can lose the signal a trap with a command in it was to take.
# When the signal comes just before bash looks for the end of a $( ) in a word, bash runs the trap there and reads the
# trap's own text as if it stood inside that $( ): the text does not parse, its command is not run, and the script
# runs on as if nothing had been sent, or ends at once without its stop. The controller is full of $( ).
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

# Whether $1 is an IPv4 block Docker can cut job networks from: a.b.c.d/n with n from 8 to 20, so that it holds at
# least sixteen networks of 256 addresses.
network_block_ok() {
  local block=$1 prefix part
  case $block in *[!0-9./]* | */*/* | '') return 1 ;; esac
  prefix=${block##*/}
  [ "$prefix" != "$block" ] || return 1
  case $prefix in '' | ???*) return 1 ;; esac
  [ "$((10#$prefix))" -ge 8 ] && [ "$((10#$prefix))" -le 20 ] || return 1
  local address=${block%/*} count=0
  case $address in *..* | .* | *.) return 1 ;; esac
  local IFS=.
  for part in $address; do
    case $part in '' | ????*) return 1 ;; esac
    [ "$((10#$part))" -le 255 ] || return 1
    count=$((count + 1))
  done
  [ "$count" = 4 ]
}

install_pool() {
  export DEBIAN_FRONTEND=noninteractive
  local try missing block
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
    # Every job gets a network of its own, and Docker cuts those from this block. Docker's own default takes them
    # from the private ranges most local networks use, where a GitLab on a private network often is: a job network
    # over GitLab's own address would hide GitLab from the pool. A block the settings do not give right is said by the controller,
    # which then does not start; the daemon gets the template's own.
    block=$(jq -r '.jobNetworks // empty' "$POOL_SETTINGS" 2>/dev/null)
    network_block_ok "$block" || block=10.213.0.0/16
    printf '{ "log-driver": "local", "log-opts": { "max-size": "20m", "max-file": "2" }, "default-address-pools": [ { "base": "%s", "size": 24 } ] }\n' "$block" \
      >/etc/docker/daemon.json
  fi
  install -m 0755 "$0" "$INSTALLED"
  cat >/etc/systemd/system/arca-gitlab-pool.service <<UNIT
[Unit]
Description=Arca GitLab Runner pool
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
  if ! systemctl restart docker || ! systemctl enable --now arca-gitlab-pool.service; then
    boot_say "first boot failed: Docker or the controller's service did not start. Inside the machine, 'systemctl status docker arca-gitlab-pool' says why. The machine has no pool"
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
  # One to six digits, and not zero: six digits of the largest unit still fit the shell's arithmetic, and a job of
  # no memory is no job.
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

# A whole number of at most $2 digits, without leading zeros: prints it, or fails. A number is checked for its length
# before it is used: one too long for the shell's arithmetic is an error there, not a number.
whole() {
  local value=$1 digits=$2
  case $value in '' | *[!0-9]*) return 1 ;; esac
  [ ${#value} -le "$digits" ] || return 1
  echo $((10#$value))
}

# An image named by digest and by nothing else: <name>@sha256:<64 hex digits>.
pinned() {
  local image=$1 digest
  case $image in *@sha256:*) ;; *) return 1 ;; esac
  digest=${image##*@sha256:}
  case ${image%@sha256:*} in '' | *[!A-Za-z0-9._/:-]*) return 1 ;; esac
  [ ${#digest} = 64 ] && [ -z "${digest//[0-9a-f]/}" ]
}

R_NAME=()
R_LIMIT=()
R_CPU=()
R_MEM=()
R_STATE=()

# Reads the settings. A setting that cannot be used is said in words and the controller does not start.
load_settings() {
  if ! jq -e 'type == "object"' "$POOL_SETTINGS" >/dev/null 2>&1; then
    say "settings: $POOL_SETTINGS is missing or is not a JSON object"
    return 1
  fi
  setting() { jq -r "$1 // empty | tostring" "$POOL_SETTINGS"; }
  URL=$(setting .url)
  URL=${URL%/}
  if [ -z "$URL" ]; then
    say "settings: url is not set. It is the address of your GitLab, for example https://gitlab.example.com: create the pool with it in settings"
    return 1
  fi
  case $URL in
    https://*) ;;
    *)
      say "settings: url must begin https://, for example https://gitlab.example.com"
      return 1
      ;;
  esac
  local rest host path hostname port=''
  rest=${URL#https://}
  host=${rest%%/*}
  path=${rest#"$host"}
  # The tokens are sent to this host, so it is a plain host name and nothing else: letters, digits, dots and dashes,
  # with a port at most. An "@" would make what stands before it a user name and what follows the real host; a "#"
  # or a "?" would end the host early.
  hostname=${host%%:*}
  [ "$hostname" = "$host" ] || port=${host#*:}
  case $hostname in
    '' | *[!A-Za-z0-9.-]* | [.-]* | *[.-] | *..*)
      say "settings: url: its host must be a plain host name (letters, digits, dots and dashes), with no user, password, query or fragment: the pool's tokens are sent to it"
      return 1
      ;;
  esac
  if [ "$hostname" != "$host" ]; then
    case $port in '' | *[!0-9]* | ??????*)
      say "settings: url: its host must be a plain host name (letters, digits, dots and dashes), with no user, password, query or fragment: the pool's tokens are sent to it"
      return 1
      ;;
    esac
  fi
  case $path in *[!A-Za-z0-9._~/-]* | *//*)
    say "settings: url is the address of the GitLab itself, such as https://gitlab.example.com or https://example.com/gitlab: its path is letters, digits, dots, dashes and slashes"
    return 1
    ;;
  esac
  HOST=$hostname
  API_URL="$URL/api/v4"
  # The test's fake GitLab. Only root on the machine can set it, in the controller's own unit.
  [ -z "${POOL_API_URL:-}" ] || API_URL=${POOL_API_URL%/}
  # Where the token check connects, for the words of a failure.
  rest=${API_URL#*://}
  rest=${rest%%/*}
  CHECK_HOST=${rest%%:*}
  if [ "$CHECK_HOST" != "$rest" ]; then
    CHECK_PORT=${rest#*:}
  else
    case $API_URL in https://*) CHECK_PORT=443 ;; *) CHECK_PORT=80 ;; esac
  fi

  local reserve service_memory free_disk
  CONCURRENT=$(whole "$(setting .concurrent)" 3) || CONCURRENT=''
  JOB_CPU=$(whole "$(setting .jobCpu)" 3) || JOB_CPU=''
  JOB_SERVICES=$(whole "$(setting .jobServices)" 2) || JOB_SERVICES=''
  SERVICE_CPU=$(whole "$(setting .serviceCpu)" 3) || SERVICE_CPU=''
  if [ -z "$CONCURRENT" ] || [ "$CONCURRENT" -lt 1 ] || [ -z "$JOB_CPU" ] || [ "$JOB_CPU" -lt 1 ] ||
    [ -z "$JOB_SERVICES" ] || [ -z "$SERVICE_CPU" ] || [ "$SERVICE_CPU" -lt 1 ]; then
    say "settings: concurrent (jobs at once, 1 or more), jobCpu and serviceCpu (CPUs, 1 or more) and jobServices (services a job may start, 0 or more) are whole numbers"
    return 1
  fi
  JOB_MEM=$(size_bytes "$(setting .jobMemory)") || JOB_MEM=''
  service_memory=$(size_bytes "$(setting .serviceMemory)") || service_memory=''
  reserve=$(size_bytes "$(setting .reserveMemory)") || reserve=''
  free_disk=$(size_bytes "$(setting .freeDisk)") || free_disk=''
  if [ -z "$JOB_MEM" ] || [ -z "$service_memory" ] || [ -z "$reserve" ] || [ -z "$free_disk" ]; then
    say "settings: jobMemory, serviceMemory, reserveMemory and freeDisk are sizes such as 2GiB"
    return 1
  fi
  SERVICE_MEM=$service_memory
  RESERVE=$reserve
  # How far the jobs' memory caps may add up beyond what the machine has for jobs, in percent: 100 is not at all.
  OVERCOMMIT=$(whole "$(jq -r '.memoryOvercommitPercent // 100 | tostring' "$POOL_SETTINGS")" 3) || OVERCOMMIT=''
  if [ -z "$OVERCOMMIT" ] || [ "$OVERCOMMIT" -lt 100 ] || [ "$OVERCOMMIT" -gt 200 ]; then
    say "settings: memoryOvercommitPercent is a whole number from 100 (the jobs' memory caps add up to no more than the machine has for jobs) to 200 (to twice that)"
    return 1
  fi
  FREE_DISK=$free_disk
  REACH=$(jq -r '.reach | if . == null then "direct" elif type == "string" then . else "!" end' "$POOL_SETTINGS")
  if [ "$REACH" != direct ] && [ "$REACH" != tailnet ]; then
    say "settings: reach is how the machine reaches GitLab: \"direct\" (over the machine's own network; also when it is left out) or \"tailnet\" (the machine joins your tailnet with TAILSCALE_AUTHKEY)"
    return 1
  fi
  # With reach "tailnet": the tailnet's own name of GitLab's node, for a url whose name only the tailnet's DNS knows.
  GITLAB_NODE=$(jq -r '.gitlabNode | if . == null then "" elif type == "string" then . else "!" end' "$POOL_SETTINGS")
  case $GITLAB_NODE in *[!A-Za-z0-9.-]* | [.-]* | *..*) GITLAB_NODE='!' ;; esac
  if [ "$GITLAB_NODE" = '!' ] || [ ${#GITLAB_NODE} -gt 253 ]; then
    say "settings: gitlabNode is the name of GitLab's node on your tailnet (its machine name there, or its full tailnet name): letters, digits, dots and dashes"
    return 1
  fi
  if [ -n "$GITLAB_NODE" ] && [ "$REACH" != tailnet ]; then
    say "settings: gitlabNode goes with reach \"tailnet\": it names GitLab's node on the tailnet the machine joins. With reach \"direct\" leave it empty"
    return 1
  fi
  # Privileged job containers are off unless the settings say so in so many words: true, the JSON value.
  PRIVILEGED=$(jq -r '.privileged | if . == null or . == false then "false" elif . == true then "true" else "!" end' "$POOL_SETTINGS")
  if [ "$PRIVILEGED" = '!' ]; then
    say "settings: privileged is true or false. It is false unless a template sets it: a privileged job container can take over the machine, and with it every token of the pool"
    return 1
  fi
  RUNNER_IMAGE=$(setting .runnerImage)
  HELPER_IMAGE=$(setting .helperImage)
  TAILNET_IMAGE=$(setting .tailnetImage)
  local key
  for key in runnerImage:"$RUNNER_IMAGE" helperImage:"$HELPER_IMAGE" tailnetImage:"$TAILNET_IMAGE"; do
    if ! pinned "${key#*:}"; then
      say "settings: ${key%%:*} must name its image by digest, <name>@sha256:<64 hex digits>: the pool runs no image it cannot name exactly"
      return 1
    fi
  done
  JOB_NETWORKS=$(setting .jobNetworks)
  if ! network_block_ok "$JOB_NETWORKS"; then
    say "settings: jobNetworks is the block of private addresses the jobs' own networks are cut from, such as 10.213.0.0/16: an IPv4 block of /8 to /20"
    return 1
  fi

  local name limit cpu memory bytes n=0 seen=' '
  while IFS=$'\t' read -r name limit cpu memory; do
    n=$((n + 1))
    limit=$(whole "$limit" 3) || limit=''
    cpu=$(whole "$cpu" 3) || cpu=''
    bytes=0
    if [ -n "$memory" ]; then bytes=$(size_bytes "$memory") || bytes=''; fi
    if [ -z "$name" ] || [ -n "${name//[a-z0-9-]/}" ] || [ ${#name} -gt 32 ] || [ -z "${name%%[!a-z]*}" ] ||
      [ -z "$limit" ] || [ -z "$cpu" ] || [ -z "$bytes" ]; then
      say "settings: runners $n needs a name (a-z, 0-9 and dashes, beginning with a letter, at most 32) and may give limit (jobs at once for this registration; 0 for no limit of its own), jobCpu (0 for the pool's) and jobMemory (a size such as 2GiB; \"\" for the pool's)"
      return 1
    fi
    case $seen in *" $name "*)
      say "settings: runners $n has the name of an earlier entry ($name): each registration has a name of its own"
      return 1
      ;;
    esac
    seen="$seen$name "
    [ "$cpu" -gt 0 ] || cpu=$JOB_CPU
    [ "$bytes" -gt 0 ] || bytes=$JOB_MEM
    R_NAME+=("$name")
    R_LIMIT+=("$limit")
    R_CPU+=("$cpu")
    R_MEM+=("$bytes")
    R_STATE+=(unknown)
  done < <(jq -r '.runners | if type == "array" then .[] else empty end | if type == "object" then . else {} end
    | [((.name // "") | tostring | if . == "" then "-" else . end), ((.limit // 0) | tostring), ((.jobCpu // 0) | tostring), ((.jobMemory // "") | tostring)] | @tsv' "$POOL_SETTINGS")
  if [ ${#R_NAME[@]} -lt 1 ]; then
    say "settings: runners needs at least one entry: one for each runner you created in GitLab, each with a name"
    return 1
  fi
  if [ ${#R_NAME[@]} -gt "$MAX_RUNNERS" ]; then
    say "settings: runners has ${#R_NAME[@]} entries and one pool takes at most $MAX_RUNNERS: each has a secret of its own, GITLAB_RUNNER_TOKEN_1 to GITLAB_RUNNER_TOKEN_$MAX_RUNNERS. Create a second pool for the others"
    return 1
  fi
  POOL_NAME=${POOL_NAME:-$(hostname 2>/dev/null)}
  POOL_NAME=$(printf '%s' "$POOL_NAME" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9-' '-' | cut -c1-20)
  : "${POOL_NAME:=pool}"
  RUNNER="$POOL_NAME-gitlab-runner"
  TAILNET="$POOL_NAME-tailnet"
}

# The id GitLab knows this machine's GitLab Runner by, among the machines that share a token: made once and kept on
# the pool's disk, in the form GitLab Runner writes its own (r_ and twelve letters or digits). The controller gives
# GitLab Runner the same one, so the token check and the runner are one machine to GitLab.
load_system_id() {
  local file="$POOL_STATE/system-id" rest
  SYSTEM_ID=$(cat "$file" 2>/dev/null)
  rest=${SYSTEM_ID#r_}
  if [ "$rest" != "$SYSTEM_ID" ] && [ ${#rest} = 12 ] && [ -z "${rest//[0-9a-zA-Z]/}" ]; then
    return 0
  fi
  SYSTEM_ID="r_$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
  printf '%s\n' "$SYSTEM_ID" >"$file.new" && mv "$file.new" "$file"
}

meminfo_bytes() { awk -v key="$1:" '$1 == key { printf "%d", $2 * 1024 }' "$POOL_MEMINFO"; }

# Whether the jobs the settings allow at once fit the machine, by what the machine itself reports. A job takes its
# registration's jobMemory, and serviceMemory for each of the jobServices it may start. The worst case fills the
# places of `concurrent` with the largest jobs first, each registration up to its own limit. When that, with
# reserveMemory kept back for the machine, Docker and GitLab Runner, is more than the machine has, the pool does not
# start: GitLab Runner admits jobs by their number, not by memory, so nothing else would hold them back.
#
# memoryOvercommitPercent above 100 lets that sum pass what the machine has, up to that many percent of it. It is
# for a load whose jobs differ widely in size and cannot be told apart by their runner: the cap must then be the
# largest job's, and the sum of caps is far more than the jobs use together. Every container keeps its own cap, so
# a job that passes it is the one the kernel stops. What is given up is the promise that the machine cannot run
# out: if many large jobs peak together it can, and the kernel then picks what it stops. $OVERCOMMIT_NOTE says so
# at the start and in every status block.
# $FIT_WORDS says the numbers either way.
machine_fits() {
  local total cpus i j left=$CONCURRENT worst=0 take size pick parts='' taken=() allowed stretched=''
  total=$(meminfo_bytes MemTotal)
  total=${total:-0}
  BUDGET=$((total - RESERVE))
  allowed=$((BUDGET * OVERCOMMIT / 100))
  OVERCOMMIT_NOTE=''
  if [ "$OVERCOMMIT" != 100 ]; then
    stretched=", which memoryOvercommitPercent $OVERCOMMIT makes $(mib "$allowed") MiB"
    OVERCOMMIT_NOTE="WARNING: memoryOvercommitPercent is $OVERCOMMIT: the jobs' memory caps may add up to $((OVERCOMMIT / 100)).$(((OVERCOMMIT % 100) / 10))$((OVERCOMMIT % 10)) times the $(mib "$BUDGET") MiB the machine has for jobs. Every container keeps its own cap, so a job that passes its cap is the one that is stopped, not its neighbours. But if many large jobs peak together the machine itself can run out of memory, and the kernel then picks what it stops"
  fi
  # No size is multiplied before it is known to be at most the machine's whole memory: a size of six digits of the
  # largest unit, times the jobs at once, would pass what the shell can count, and a sum that wraps around would
  # read as one that fits.
  if [ "$RESERVE" -ge "$total" ]; then
    FIT_WORDS="the pool does not start: reserveMemory keeps $(mib "$RESERVE") MiB back and the whole machine reports $(mib "$total") MiB, so nothing is left for jobs"
    return 1
  fi
  for i in "${!R_NAME[@]}"; do
    if [ "${R_MEM[$i]}" -gt "$total" ]; then
      FIT_WORDS="the pool does not start: runners $((i + 1)) (${R_NAME[$i]}) gives each job $(mib "${R_MEM[$i]}") MiB of memory (jobMemory) and the whole machine reports $(mib "$total") MiB. Give a job at most what the machine has for jobs, $(mib "$BUDGET") MiB, and create the pool again"
      return 1
    fi
  done
  if [ "$JOB_SERVICES" -gt 0 ] && [ "$SERVICE_MEM" -gt "$total" ]; then
    FIT_WORDS="the pool does not start: serviceMemory gives each service $(mib "$SERVICE_MEM") MiB and the whole machine reports $(mib "$total") MiB"
    return 1
  fi
  cpus=${POOL_CPUS:-$(nproc 2>/dev/null)}
  case $cpus in '' | *[!0-9]*) cpus=0 ;; esac
  for i in "${!R_NAME[@]}"; do
    if [ "$cpus" -gt 0 ] && [ "${R_CPU[$i]}" -gt "$cpus" ]; then
      FIT_WORDS="the pool does not start: runners $((i + 1)) (${R_NAME[$i]}) gives each job ${R_CPU[$i]} CPUs and the machine has $cpus, so Docker would refuse every job's container. Give jobCpu at most $cpus and create the pool again"
      return 1
    fi
  done
  if [ "$cpus" -gt 0 ] && [ "$JOB_SERVICES" -gt 0 ] && [ "$SERVICE_CPU" -gt "$cpus" ]; then
    FIT_WORDS="the pool does not start: serviceCpu gives each service $SERVICE_CPU CPUs and the machine has $cpus, so Docker would refuse every service's container"
    return 1
  fi
  for i in "${!R_NAME[@]}"; do taken[i]=''; done
  # At most twelve registrations: the largest job not yet counted is found by looking at all of them.
  while [ "$left" -gt 0 ]; do
    pick=''
    for j in "${!R_NAME[@]}"; do
      [ -z "${taken[$j]}" ] || continue
      if [ -z "$pick" ] || [ "${R_MEM[$j]}" -gt "${R_MEM[$pick]}" ]; then pick=$j; fi
    done
    [ -n "$pick" ] || break
    taken[pick]=1
    take=${R_LIMIT[$pick]}
    if [ "$take" = 0 ] || [ "$take" -gt "$left" ]; then take=$left; fi
    size=$((R_MEM[pick] + JOB_SERVICES * SERVICE_MEM))
    worst=$((worst + take * size))
    left=$((left - take))
    parts="${parts:+$parts, }$take of ${R_NAME[$pick]} at $(mib "$size") MiB"
  done
  JOBS_MOST=$((CONCURRENT - left))
  if [ "$worst" -gt "$allowed" ]; then
    FIT_WORDS="the pool does not start: its jobs could take $(mib "$worst") MiB at once ($parts) and the machine has $(mib "$BUDGET") MiB for jobs (the $(mib "$total") MiB it reports, less reserveMemory $(mib "$RESERVE") MiB)$stretched: $(mib $((worst - allowed))) MiB too much. A job takes its jobMemory and, for each of the $JOB_SERVICES services it may start, serviceMemory; a machine reports less memory than the size it was given. Lower concurrent, a registration's limit, jobMemory or serviceMemory and create the pool again; a larger machine is gitlab-runner-pool-large, or a copy of the template with larger limits"
    return 1
  fi
  if [ "$worst" -gt "$BUDGET" ]; then
    FIT_WORDS="at most $JOBS_MOST jobs at once, $(mib "$worst") MiB where the machine has $(mib "$BUDGET") MiB for jobs ($parts): more than it has, as memoryOvercommitPercent $OVERCOMMIT allows (up to $(mib "$allowed") MiB)"
  else
    FIT_WORDS="at most $JOBS_MOST jobs at once, $(mib "$worst") MiB of the $(mib "$BUDGET") MiB the machine has for jobs ($parts)"
  fi
  return 0
}

# ---------------------------------------------------------------------------------------------------------------------
# Secrets and GitLab
# ---------------------------------------------------------------------------------------------------------------------

# The secret of the registration at place $1, counted from 1 as the owner's card counts the entries of runners.
token_file() { printf '%s/GITLAB_RUNNER_TOKEN_%s' "$POOL_SECRETS" "$1"; }

# What is not set yet, in words that name each secret; nothing when every registration has its token (and, with reach
# "tailnet", the machine has its key).
missing_secrets() {
  local i words=''
  for i in "${!R_NAME[@]}"; do
    [ -s "$(token_file $((i + 1)))" ] ||
      words="${words:+$words; }runners $((i + 1)) (${R_NAME[$i]}) needs GITLAB_RUNNER_TOKEN_$((i + 1))"
  done
  if [ "$REACH" = tailnet ] && [ ! -s "$POOL_SECRETS/TAILSCALE_AUTHKEY" ]; then
    words="${words:+$words; }reach \"tailnet\" needs TAILSCALE_AUTHKEY"
  fi
  printf '%s' "$words"
}

# One number for the secrets as they are now: it changes when Arca writes another value.
secrets_tag() {
  local i
  {
    for i in "${!R_NAME[@]}"; do cat "$(token_file $((i + 1)))" 2>/dev/null; echo "$i"; done
    cat "$POOL_SECRETS/TAILSCALE_AUTHKEY" 2>/dev/null
  } | sha256sum | cut -c1-32
}

# The address GitLab's name resolves to from this machine, or nothing. With reach "tailnet" and a GitLab that is a
# node of the tailnet it is the address the tailnet gave, which the pool pins.
resolved_address() {
  if [ -n "${GITLAB_PIN:-}" ]; then
    printf '%s' "$GITLAB_PIN"
    return 0
  fi
  case $CHECK_HOST in *[!0-9.]*) ;; *)
    printf '%s' "$CHECK_HOST"
    return 0
    ;;
  esac
  timeout 10 getent hosts "$CHECK_HOST" 2>/dev/null | awk 'NR == 1 { printf "%s", $1 }'
}

# What to do about a name the machine's DNS does not know, for the reach the pool has. A pool that is already on the
# tailnet is not told to join it: there the name is neither a node's own name nor one ordinary DNS answers.
unresolved_advice() {
  if [ "$REACH" = tailnet ]; then
    echo "The machine is on the tailnet, and the name is neither the name of one of its nodes nor one that ordinary DNS answers. If only the tailnet's own DNS knows it, create the pool with settings.gitlabNode, the name of GitLab's node on the tailnet"
  else
    echo "A GitLab on a private network or a tailnet has a name only that network's DNS answers: the machine's network must lead there, or the pool is created with reach \"tailnet\""
  fi
}

# Why GitLab could not be asked, from curl's exit status: one sentence for each thing that can stand between the
# machine and GitLab, so the owner knows which one to look at.
unreachable_words() {
  local status=$1 connected=${2-} address
  address=$(resolved_address)
  case $status in
    6)
      echo "GitLab cannot be reached: the name $CHECK_HOST does not resolve from this machine, so its DNS does not know it. $(unresolved_advice)"
      ;;
    7)
      echo "GitLab cannot be reached: $CHECK_HOST resolves to ${address:-an address} and refuses the connection on port $CHECK_PORT. Nothing listens there, or a firewall answers for it"
      ;;
    28)
      if [ -n "$connected" ]; then
        echo "GitLab cannot be reached: $CHECK_HOST accepts the connection on port $CHECK_PORT and did not answer within $POOL_HTTP_WAIT seconds. It is reached and does not answer: GitLab itself, or a proxy in front of it, is stuck or overloaded"
      elif [ -n "$address" ]; then
        echo "GitLab cannot be reached: $CHECK_HOST resolves to $address and the connection to port $CHECK_PORT timed out after $POOL_CONNECT_WAIT seconds. The name is known and nothing answers: a firewall or an access rule between this machine and GitLab drops it (on a tailnet, the rule that lets this machine, or the node it goes out through, reach GitLab)"
      else
        echo "GitLab cannot be reached: the name $CHECK_HOST does not resolve from this machine (the lookup timed out), so its DNS does not know it or does not answer. $(unresolved_advice)"
      fi
      ;;
    51 | 60 | 77 | 83 | 90 | 91)
      echo "GitLab cannot be reached: $CHECK_HOST answers on port $CHECK_PORT and its certificate is refused (curl exit $status). It is not signed by an authority this machine trusts, or it is for another name. This pool trusts the public authorities only: a GitLab with a certificate of a private authority is not served by it"
      ;;
    35 | 53 | 54 | 58 | 59 | 66 | 80 | 82)
      echo "GitLab cannot be reached: $CHECK_HOST answers on port $CHECK_PORT and no TLS connection could be made (curl exit $status). What listens there is not HTTPS, or offers no TLS this machine accepts"
      ;;
    *)
      echo "GitLab cannot be reached at $CHECK_HOST port $CHECK_PORT (curl exit $status)"
      ;;
  esac
}

# Asks GitLab whether it takes the token of the registration at place $1 (from 1). Returns 0 when it does, 1 when the
# token is refused (only another token ends that), and 2 when GitLab could not be asked or answered something else.
# $CHECK_WORDS says what happened, without the token. The token goes to curl on its standard input; GitLab's answer
# repeats the token, so it is not kept.
check_token() {
  local place=$1 token code status connected pin=()
  CHECK_WORDS=''
  token=$(tr -d ' \t\r\n' <"$(token_file "$place")" 2>/dev/null)
  case $token in
    glrt-*) ;;
    *)
      CHECK_WORDS="GITLAB_RUNNER_TOKEN_$place is not a runner authentication token. Such a token begins glrt-, and GitLab shows it once, when the runner is created (CI/CD, Runners, New runner); a registration token of the older kind is not taken. Set the right one (service_secret_set, then instance_start)"
      return 1
      ;;
  esac
  case $token in *[!A-Za-z0-9._-]*)
    CHECK_WORDS="GITLAB_RUNNER_TOKEN_$place has a character no runner authentication token has. Set it again as GitLab showed it (service_secret_set, then instance_start)"
    return 1
    ;;
  esac
  [ -z "${GITLAB_PIN:-}" ] || pin=(--resolve "$CHECK_HOST:$CHECK_PORT:$GITLAB_PIN")
  code=$(printf '{"token":"%s","system_id":"%s"}' "$token" "$SYSTEM_ID" |
    curl -sS -o /dev/null -w '%{http_code} %{time_connect}' --connect-timeout "$POOL_CONNECT_WAIT" --max-time "$POOL_HTTP_WAIT" \
      -X POST -H 'Content-Type: application/json' -H 'User-Agent: arca-gitlab-pool' --data-binary @- \
      "${pin[@]}" "$API_URL/runners/verify" 2>/dev/null)
  status=$?
  token=''
  # curl says how long the connection took to make: nothing but zeros when it was never made.
  connected=${code##* }
  code=${code%% *}
  [ -n "${connected//[0.]/}" ] || connected=''
  if [ "$status" != 0 ]; then
    CHECK_WORDS=$(unreachable_words "$status" "$connected")
    return 2
  fi
  case $code in
    200) return 0 ;;
    403)
      CHECK_WORDS="GitLab answers and does not take GITLAB_RUNNER_TOKEN_$place (403): the token is not the one GitLab holds for a runner, it was reset, or its runner was deleted. Create the runner again in GitLab, or reset its token there, and set the new one (service_secret_set, then instance_start)"
      return 1
      ;;
    404)
      CHECK_WORDS="GitLab answered 404 to the token check at $URL: url is not the address of a GitLab. It is the address of the GitLab itself, not of a group or a project"
      return 2
      ;;
    *)
      CHECK_WORDS="GitLab answered $code to the token check at $URL: it is reached and does not answer as a GitLab that is well"
      return 2
      ;;
  esac
}

# Asks GitLab about every registration's token and keeps the answers in R_STATE: `ok`, or the words of a refusal. A
# refusal is said when it is new, and so is a token that is taken again. Returns 0 when GitLab answered for every
# token, also with a refusal, and 2 as soon as GitLab could not be asked: the registrations then stay as they were,
# and $CHECK_WORDS says why.
check_tokens() {
  local i verdict
  for i in "${!R_NAME[@]}"; do
    # A stop is read between two registrations: each check may take as long as GitLab is given to answer, and twelve
    # of them are more than a stop is given.
    ! stopping || return 2
    check_token $((i + 1))
    verdict=$?
    [ "$verdict" != 2 ] || return 2
    if [ "$verdict" = 0 ]; then
      case ${R_STATE[$i]} in ok | unknown) ;; *) say "runners $((i + 1)) (${R_NAME[$i]}): GitLab takes its token now" ;; esac
      R_STATE[i]=ok
    else
      [ "${R_STATE[$i]}" = "$CHECK_WORDS" ] || {
        printf x >>"$POOL_RUN/failures"
        say "runners $((i + 1)) (${R_NAME[$i]}) is left out: $CHECK_WORDS"
      }
      R_STATE[i]=$CHECK_WORDS
    fi
  done
  return 0
}

accepted() {
  local i count=0
  for i in "${!R_NAME[@]}"; do [ "${R_STATE[$i]}" != ok ] || count=$((count + 1)); done
  echo "$count"
}

# ---------------------------------------------------------------------------------------------------------------------
# GitLab Runner
# ---------------------------------------------------------------------------------------------------------------------

# GitLab Runner's configuration, for the registrations GitLab takes. It holds their tokens, so the file is made 0600
# in a directory only root enters, and it is written beside its place and renamed in: GitLab Runner reads the file
# again by itself when it changes, and never reads half of one.
#
# For every registration: the docker executor; no privileged container and no host path or socket in any job; a
# network of its own for each job (set here, where a job cannot turn it off), so one job cannot reach another job's
# service; nothing of a job kept for the next (no cache volume); a memory cap without swap and a CPU ceiling on the
# job's container and on each service; and GitLab Runner's helper image by digest.
write_config() {
  local i token limit
  mkdir -p "$POOL_CONFIG" || return 1
  chmod 700 "$POOL_CONFIG" || return 1
  (
    umask 077
    printf '%s\n' "$SYSTEM_ID" >"$POOL_CONFIG/.runner_system_id" || exit 1
    {
      printf 'concurrent = %s\ncheck_interval = 3\nshutdown_timeout = 30\nlog_format = "json"\nlog_level = "info"\n' "$CONCURRENT"
      for i in "${!R_NAME[@]}"; do
        [ "${R_STATE[$i]}" = ok ] || continue
        token=$(tr -d ' \t\r\n' <"$(token_file $((i + 1)))" 2>/dev/null)
        # Checked again here: only a token of the plain characters is written between the quotes.
        case $token in '' | *[!A-Za-z0-9._-]*) continue ;; esac
        limit=${R_LIMIT[$i]}
        printf '\n[[runners]]\n  name = "%s"\n  url = "%s"\n  token = "%s"\n  executor = "docker"\n  limit = %s\n' \
          "${R_NAME[$i]}" "$URL" "$token" "$limit"
        printf '  [runners.feature_flags]\n    FF_NETWORK_PER_BUILD = true\n'
        printf '  [runners.docker]\n    helper_image = "%s"\n    privileged = %s\n    services_privileged = %s\n' \
          "$HELPER_IMAGE" "$PRIVILEGED" "$PRIVILEGED"
        printf '    disable_cache = true\n    volumes = []\n    pull_policy = ["always", "if-not-present"]\n'
        printf '    memory = "%sm"\n    memory_swap = "%sm"\n    cpus = "%s"\n' \
          "$(mib "${R_MEM[$i]}")" "$(mib "${R_MEM[$i]}")" "${R_CPU[$i]}"
        printf '    services_limit = %s\n    service_memory = "%sm"\n    service_memory_swap = "%sm"\n    service_cpus = "%s"\n' \
          "$JOB_SERVICES" "$(mib "$SERVICE_MEM")" "$(mib "$SERVICE_MEM")" "$SERVICE_CPU"
        [ -z "${GITLAB_PIN:-}" ] || printf '    extra_hosts = ["%s:%s"]\n' "$HOST" "$GITLAB_PIN"
      done
    } >"$POOL_CONFIG/config.toml.new" || exit 1
    token=''
    mv -f "$POOL_CONFIG/config.toml.new" "$POOL_CONFIG/config.toml"
  )
}

# GitLab Runner itself: one container from the pinned image. It alone has the Docker daemon's socket, which it
# creates the jobs' containers through, and the configuration with the tokens. No job container gets either.
start_runner() {
  local hosts=()
  [ -z "${GITLAB_PIN:-}" ] || hosts=(--add-host "$HOST:$GITLAB_PIN")
  docker rm -f "$RUNNER" >/dev/null 2>&1
  JOBS_CURSOR=$(($(now) - 2))
  docker run -d --name "$RUNNER" --label "arca.pool=$POOL_NAME" --restart no \
    -v "$POOL_CONFIG:/etc/gitlab-runner" -v "$POOL_DOCKER_SOCKET:/var/run/docker.sock" \
    "${hosts[@]}" "$RUNNER_IMAGE" >/dev/null 2>&1
}

# `true <code>` or `false <code>`; `false 255` for a container that is not there.
container_state() {
  docker inspect -f '{{.State.Running}} {{.State.ExitCode}}' "$1" 2>/dev/null || echo 'false 255'
}

# Pulls the pinned images: GitLab Runner, its helper (so the first job does not wait for it) and, with reach
# "tailnet", the tailnet node.
prepare_images() {
  local image images=("$RUNNER_IMAGE" "$HELPER_IMAGE")
  [ "$REACH" != tailnet ] || images+=("$TAILNET_IMAGE")
  for image in "${images[@]}"; do
    until docker image inspect "$image" >/dev/null 2>&1 || docker pull -q "$image" >/dev/null 2>&1; do
      ! stopping || return 1
      say "the image $image could not be pulled; trying again in 30 seconds"
      nap 30
      ! stopping || return 1
    done
  done
}

# What an earlier controller left on this machine: its own containers, and the containers, volumes and networks of
# jobs that were running when it ended. Nothing of a job outlives the controller that ran it.
clean_leftovers() {
  local ids
  ids=$(
    docker ps -aq --filter "label=arca.pool=$POOL_NAME" 2>/dev/null
    docker ps -aq --filter label=com.gitlab.gitlab-runner.managed=true 2>/dev/null
  )
  # shellcheck disable=SC2086
  [ -z "$ids" ] || docker rm -fv $ids >/dev/null 2>&1
  docker volume prune -af >/dev/null 2>&1
  docker network prune -f >/dev/null 2>&1
}

# ---------------------------------------------------------------------------------------------------------------------
# The tailnet (settings.reach "tailnet")
# ---------------------------------------------------------------------------------------------------------------------

# The tailnet node: Tailscale's own image, by digest, in the machine's own network, where it makes the tailnet's
# interface and routes. The machine then reaches the tailnet, and so does every container on it: GitLab Runner and
# each job, whose connections leave through the machine as all their connections do. The node is started as
# Tailscale documents it for a machine's own network: the capability to make an interface and its routes, and the
# device an interface is made from. Its key is a file it is told the path of; its identity is kept on the pool's
# disk, so a restart does not use the key again.
tailnet_start() {
  local dir="$POOL_RUN/tailnet"
  docker rm -f "$TAILNET" >/dev/null 2>&1
  GITLAB_PIN=''
  mkdir -p "$dir" "$POOL_STATE/tailnet" && chmod 700 "$dir" "$POOL_STATE/tailnet" || return 1
  if ! (umask 077 && tr -d ' \t\r\n' <"$POOL_SECRETS/TAILSCALE_AUTHKEY" >"$dir/authkey.new") ||
    ! chmod 0400 "$dir/authkey.new" || ! mv -f "$dir/authkey.new" "$dir/authkey"; then
    rm -f "$dir/authkey.new"
    return 1
  fi
  TAILNET_KEY=$(sha256sum <"$POOL_SECRETS/TAILSCALE_AUTHKEY" | cut -c1-32)
  docker run -d --name "$TAILNET" --label "arca.pool=$POOL_NAME" --network host --restart no \
    --cap-add NET_ADMIN --device /dev/net/tun \
    -v "$dir:/run/arca-tailnet:ro" -v "$POOL_STATE/tailnet:/var/lib/tailscale" \
    -e TS_AUTHKEY=file:/run/arca-tailnet/authkey -e TS_AUTH_ONCE=true -e TS_USERSPACE=false \
    -e TS_STATE_DIR=/var/lib/tailscale -e "TS_SOCKET=$TAILNET_SOCKET" -e "TS_HOSTNAME=$POOL_NAME" \
    -e TS_ACCEPT_DNS=false "$TAILNET_IMAGE" >/dev/null 2>&1
}

tailnet_cli() { docker exec "$TAILNET" tailscale --socket "$TAILNET_SOCKET" "$@" 2>/dev/null; }

ipv4() {
  case $1 in '' | *[!0-9.]* | *..* | .* | *.) return 1 ;; esac
  local IFS=. part count=0
  for part in $1; do
    case $part in '' | ????*) return 1 ;; esac
    [ "$((10#$part))" -le 255 ] || return 1
    count=$((count + 1))
  done
  [ "$count" = 4 ]
}

# The IPv4 address the tailnet gives the node named $1, read from the tailnet's own status ($2, the answer of
# `tailscale status --json`), or nothing. The status lists every node with its full name, which ends with a dot, and
# its addresses. A name with a dot in it is a full name and must be one, whatever its case and with or without the
# last dot; a name without a dot is the node's machine name, the full name's first part. (`tailscale ip <name>` is
# not asked: it takes a full name only with its last dot, which no URL has.)
tailnet_address() {
  printf '%s' "$2" | jq -r --arg name "$1" '
    ($name | ascii_downcase | rtrimstr(".")) as $wanted
    | [(.Peer // {} | .[]), (.Self // empty)]
    | map(select(type == "object")
        | ((.DNSName // "") | tostring | ascii_downcase | rtrimstr(".")) as $full
        | select($full != "" and ($full == $wanted or (($wanted | contains(".") | not) and ($full | split(".") | .[0]) == $wanted))))
    | (.[0].TailscaleIPs // []) | map(tostring | select(test("^[0-9]+\\.[0-9]+\\.[0-9]+\\.[0-9]+$"))) | .[0] // empty' 2>/dev/null
}

# Where GitLab is, for a machine that is on the tailnet with the status $1. One of three, and the console says which:
# - settings.gitlabNode names GitLab's node: its tailnet address is told to GitLab Runner and to every job for the
#   url's name. This is for a url whose name only the tailnet's own DNS knows; the name stays the url's, so GitLab's
#   certificate still matches. A node of that name that the tailnet does not show is an error: returns 1.
# - the url's own name is a node's name on the tailnet: its address is told the same way, so nothing depends on the
#   tailnet's DNS, which the machine does not use.
# - neither: the name is looked up in DNS like any name. That is right for a name ordinary DNS answers with GitLab's
#   tailnet address.
# $LOCATE_WORDS says it; $GITLAB_PIN is the address told, or empty; $REACH_WORDS is for the status block.
tailnet_locate() {
  local status=$1 own pin names
  own=$(printf '%s' "$status" | jq -r '(.Self.TailscaleIPs // []) | map(tostring | select(test("^[0-9.]+$"))) | .[0] // empty' 2>/dev/null)
  ipv4 "$own" || own='an address it did not name'
  GITLAB_PIN=''
  if [ -n "$GITLAB_NODE" ]; then
    pin=$(tailnet_address "$GITLAB_NODE" "$status")
    if ! ipv4 "$pin"; then
      names=$(printf '%s' "$status" | jq -r '[.Peer // {} | .[] | (.DNSName // "") | tostring | ascii_downcase | split(".") | .[0] // "" | select(. != "")] | unique | .[0:12] | join(", ")' 2>/dev/null)
      REACH_WORDS="GitLab's node $GITLAB_NODE not found on the tailnet"
      LOCATE_WORDS="the machine is on the tailnet as $own, and the tailnet shows it no node named $GITLAB_NODE (settings.gitlabNode). The nodes it shows: $(plain "${names:-none}"). The name is the node's machine name on the tailnet, or its full tailnet name; and a node is shown only when the tailnet's access rules let this machine reach it"
      return 1
    fi
    GITLAB_PIN=$pin
    REACH_WORDS="GitLab at $pin on the tailnet, the node settings.gitlabNode names"
    LOCATE_WORDS="the machine is on the tailnet as $own; settings.gitlabNode names the tailnet's node $GITLAB_NODE, at $pin: GitLab Runner and every job are told that $HOST is at that address"
    return 0
  fi
  pin=$(tailnet_address "$HOST" "$status")
  if ipv4 "$pin"; then
    GITLAB_PIN=$pin
    REACH_WORDS="GitLab at $pin on the tailnet, a node by the url's own name"
    LOCATE_WORDS="the machine is on the tailnet as $own; the url's name $HOST is the name of a node of this tailnet, at $pin: GitLab Runner and every job are told that address"
  else
    REACH_WORDS="GitLab's name looked up in DNS"
    LOCATE_WORDS="the machine is on the tailnet as $own; $HOST is not the name of a node of this tailnet, so it is looked up in DNS like any name, and must resolve there to an address this machine reaches. A name that only the tailnet's own DNS knows needs settings.gitlabNode"
  fi
  return 0
}

# Brings the node up and waits until the tailnet has taken it. A key the tailnet does not take is said, and the
# node is started again when another key is set. Returns 1 only when the controller is stopping.
tailnet_wait() {
  local waited backend state status pause=0
  while ! stopping; do
    if ! tailnet_start; then
      ! stopping || return 1
      say "the tailnet node could not be started; trying again in 30 seconds"
      nap 30
      continue
    fi
    waited=0
    backend=''
    while ! stopping && [ "$waited" -lt "$POOL_READY_WAIT" ]; do
      state=$(container_state "$TAILNET")
      [ "${state%% *}" = true ] || break
      status=$(tailnet_cli status --json)
      backend=$(printf '%s' "$status" | jq -r '.BackendState // empty' 2>/dev/null)
      case $backend in Running | NeedsLogin) break ;; esac
      nap 1
      waited=$((waited + 1))
    done
    ! stopping || return 1
    if [ "$backend" = Running ]; then
      # The node stays up while a node the settings name is looked for: it may only not have been shown yet.
      while ! stopping; do
        if tailnet_locate "$status"; then
          say "$LOCATE_WORDS"
          return 0
        fi
        printf x >>"$POOL_RUN/failures"
        pause=$((pause == 0 ? 5 : pause * 2))
        [ "$pause" -gt "$POOL_BACKOFF_MAX" ] && pause=$POOL_BACKOFF_MAX
        say "$LOCATE_WORDS. Looking again in $pause seconds"
        nap "$pause"
        ! stopping || return 1
        state=$(container_state "$TAILNET")
        [ "${state%% *}" = true ] || break
        status=$(tailnet_cli status --json)
      done
      ! stopping || return 1
      continue
    fi
    printf x >>"$POOL_RUN/failures"
    pause=$((pause == 0 ? 5 : pause * 2))
    [ "$pause" -gt "$POOL_BACKOFF_MAX" ] && pause=$POOL_BACKOFF_MAX
    if [ "$backend" = NeedsLogin ]; then
      say "the tailnet did not take TAILSCALE_AUTHKEY: the key was used already, has expired or was revoked. Set another (service_secret_set, then instance_start); trying again in $pause seconds"
    elif [ "${state%% *}" != true ]; then
      say "the tailnet node stopped (exit ${state##* }) before it was on the tailnet; trying again in $pause seconds"
    else
      say "the tailnet node was not on the tailnet within $POOL_READY_WAIT seconds; trying again in $pause seconds"
    fi
    nap "$pause"
  done
  return 1
}

# ---------------------------------------------------------------------------------------------------------------------
# Jobs and status
# ---------------------------------------------------------------------------------------------------------------------

# Text from a job, made safe for one line of a console and one field of a tab-separated file.
plain() { printf '%s' "$1" | tr -c 'A-Za-z0-9 ._:/()#@+,=-' '?' | cut -c1-120; }

count_of() { stat -c %s "$POOL_RUN/$1" 2>/dev/null || echo 0; }

# The jobs that have finished since the last look, from GitLab Runner's own output: it writes one line for every job
# that ends ("Job succeeded", or "Job failed" with GitLab's reason), with the job's id and name, its project's
# number, the registration's name and the seconds it took. Each is recorded once, by its id. Nothing here is
# measured by the pool: a job's memory and CPU are not in that output, so the pool prints none.
read_jobs() {
  local at stamp runner project job name result reason seconds index i
  at=$(now)
  docker logs --since "$JOBS_CURSOR" "$RUNNER" 2>&1 |
    jq -R -r 'fromjson? | select(type == "object") | select((.msg // "") | tostring | test("^Job (succeeded|failed)"))
      | select(.job != null)
      | [(.time // "-"), (.runner_name // "-"), (.project // "-"), .job, (.job_name // "-"), (."job-status" // "-"),
         (.failure_reason // "-"), ((.duration_s // 0) | if type == "number" then floor else 0 end)]
      | map(tostring | gsub("[\t\n\r]"; " ") | if . == "" then "-" else . end) | @tsv' 2>/dev/null |
    while IFS=$'\t' read -r stamp runner project job name result reason seconds; do
      case $job in '' | *[!0-9]*) continue ;; esac
      grep -qx "$job" "$POOL_RUN/seen" 2>/dev/null && continue
      echo "$job" >>"$POOL_RUN/seen"
      index=''
      for i in "${!R_NAME[@]}"; do [ "${R_NAME[$i]}" != "$runner" ] || index=$i; done
      [ -n "$index" ] || runner=unknown
      case $result in success | failed) ;; *) result=unknown ;; esac
      case $project in *[!0-9]*) project=0 ;; esac
      case $seconds in '' | *[!0-9]*) seconds=0 ;; esac
      stamp=$(plain "$stamp")
      name=$(plain "$name")
      reason=$(plain "$reason")
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$stamp" "$runner" "$project" "$job" "$name" "$result" "$reason" "$seconds" \
        >>"$POOL_LOGS/jobs.tsv"
      printf x >>"$POOL_RUN/jobs"
      [ "$result" = success ] || printf x >>"$POOL_RUN/failed"
      if [ -n "$index" ]; then
        printf x >>"$POOL_RUN/count/$index.jobs"
        [ "$result" = success ] || printf x >>"$POOL_RUN/count/$index.failed"
      fi
      say "job runner=$runner project=$project job=$job name=\"$name\" result=$result reason=$reason seconds=$seconds"
    done
  JOBS_CURSOR=$((at - 2))
  # The ids already recorded are only needed for as long as two looks overlap.
  if [ "$(count_of seen)" -gt 65536 ]; then
    tail -n 500 "$POOL_RUN/seen" >"$POOL_RUN/seen.new" && mv "$POOL_RUN/seen.new" "$POOL_RUN/seen"
  fi
}

# How many jobs run now: the jobs that have at least one container on the machine, counted by the label GitLab Runner
# gives every container it makes. `unknown` when Docker does not answer: no number is made up.
jobs_now() {
  local ids
  if ! ids=$(docker ps --filter label=com.gitlab.gitlab-runner.managed=true --format '{{.Label "com.gitlab.gitlab-runner.job.id"}}' 2>/dev/null); then
    echo unknown
    return 0
  fi
  printf '%s\n' "$ids" | sort -u | grep -c '^[0-9][0-9]*$'
  return 0
}

# Keeps free space on the disk that holds the jobs' images. When less than freeDisk is free, the images no container
# uses are removed (a running job keeps its own; the next job that wants one pulls it again). What it frees is said,
# and so is a disk that stays short: jobs' own files are then what fills it.
DISK_NOTE=''
keep_disk_free() {
  local free
  free=$(df -B1 --output=avail "$POOL_DOCKER_ROOT" 2>/dev/null | awk 'NR == 2 { printf "%d", $1 }')
  case $free in '' | *[!0-9]*)
    DISK_FREE=unknown
    return 0
    ;;
  esac
  DISK_FREE="$(mib "$free")MiB"
  DISK_NOTE=''
  [ "$free" -lt "$FREE_DISK" ] || return 0
  docker image prune -af >/dev/null 2>&1
  local after
  after=$(df -B1 --output=avail "$POOL_DOCKER_ROOT" 2>/dev/null | awk 'NR == 2 { printf "%d", $1 }')
  case $after in '' | *[!0-9]*) after=$free ;; esac
  DISK_FREE="$(mib "$after")MiB"
  say "disk: $(mib "$free") MiB were free, less than freeDisk ($(mib "$FREE_DISK") MiB), so the images no job uses were removed: $(mib "$after") MiB are free now"
  if [ "$after" -lt "$FREE_DISK" ]; then
    DISK_NOTE="WARNING: $(mib "$after") MiB of disk are free, less than freeDisk ($(mib "$FREE_DISK") MiB), after the unused images were removed: the jobs that run now fill it, and a job that needs more fails"
  fi
}

# Sets jobs.tsv aside as jobs.tsv.1, replacing the one set aside before, once it passes POOL_JOBS_MAX.
rotate_jobs() {
  local size
  size=$(stat -c %s "$POOL_LOGS/jobs.tsv" 2>/dev/null || echo 0)
  [ "$size" -gt "$POOL_JOBS_MAX" ] || return 0
  mv "$POOL_LOGS/jobs.tsv" "$POOL_LOGS/jobs.tsv.1"
  printf '%s\n' "$JOBS_HEADER" >>"$POOL_LOGS/jobs.tsv"
}

# The status block: one line per registration, the last finished jobs, and the totals.
print_status() {
  local i limit running
  running=$(jobs_now)
  {
    for i in "${!R_NAME[@]}"; do
      limit=${R_LIMIT[$i]}
      [ "$limit" != 0 ] || limit=none
      if [ "${R_STATE[$i]}" = ok ]; then
        say "runner $((i + 1)) ${R_NAME[$i]} limit=$limit job=${R_CPU[$i]}cpu/$(mib "${R_MEM[$i]}")MiB jobs_done=$(count_of "count/$i.jobs") jobs_failed=$(count_of "count/$i.failed")"
      else
        say "runner $((i + 1)) ${R_NAME[$i]} LEFT OUT: ${R_STATE[$i]}"
      fi
    done
    # The last finished jobs again, so they stay within reach of service_logs however many status blocks follow.
    tail -n "$POOL_STATUS_JOBS" "$POOL_LOGS/jobs.tsv" 2>/dev/null | awk -F'\t' '
      $1 != "finished" { printf "arca-gitlab-pool: recent job finished=%s runner=%s project=%s job=%s name=\"%s\" result=%s reason=%s seconds=%s\n", $1, $2, $3, $4, $5, $6, $7, $8 }'
    say "pool $POOL_NAME for $URL, reach $REACH${REACH_WORDS:+ ($REACH_WORDS)}, $FIT_WORDS"
    [ "$PRIVILEGED" != true ] || say "WARNING: privileged is on: a job's container can take over this machine, read every token of the pool and reach whatever the machine reaches"
    [ -z "$OVERCOMMIT_NOTE" ] || say "$OVERCOMMIT_NOTE"
    [ -z "$GITLAB_NOTE" ] || say "$GITLAB_NOTE"
    [ -z "$DISK_NOTE" ] || say "$DISK_NOTE"
    say "totals jobs_now=$running of $JOBS_MOST jobs_done=$(count_of jobs) jobs_failed=$(count_of failed) runners=$(accepted) of ${#R_NAME[@]} failures=$(count_of failures) disk_free=${DISK_FREE:-unknown}"
  } | tee "$POOL_LOGS/status.txt.new"
  mv "$POOL_LOGS/status.txt.new" "$POOL_LOGS/status.txt"
}

# ---------------------------------------------------------------------------------------------------------------------
# The controller
# ---------------------------------------------------------------------------------------------------------------------

# Says why the pool does not start, again every POOL_REFUSE_EVERY seconds, until the controller is stopped: the
# reason stays on the console instead of a restart loop.
hold() {
  while ! stopping; do
    say "$1"
    nap "$POOL_REFUSE_EVERY"
  done
}

# The controller: everything the pool does, as a child of run_pool. It takes no signal. It learns of a stop from the
# line, and then stops in its own order. A SIGTERM sent to this process itself ends it at once with no stop; nothing
# sends one under the unit's KillMode=mixed. It does not ignore the signal instead, because everything it starts
# would then ignore it too. GNU's `timeout`, which Debian ships, still ends its program then (it sets its own handler
# first), but the `timeout` of uutils does not: on a machine that has that one, as the machines these scripts are
# tested on do, a program under a time limit would run on.
pool() {
  trap - TERM INT
  load_settings || {
    say "the pool does not start until its settings are right: they were fixed when the machine was created, so create the pool again with the right ones"
    while ! stopping; do nap 300; done
    return 1
  }
  mkdir -p "$POOL_STATE" "$POOL_LOGS" "$POOL_RUN/tmp" "$POOL_RUN/count"
  chmod 700 "$POOL_RUN" "$POOL_STATE"
  rm -f "${POOL_RUN:?}/jobs" "${POOL_RUN:?}/failed" "${POOL_RUN:?}/failures" "${POOL_RUN:?}/seen" "${POOL_RUN:?}"/count/* "${POOL_RUN:?}"/tmp/*
  load_system_id
  JOBS_HEADER=$(printf 'finished\trunner\tproject\tjob\tname\tresult\treason\tseconds')
  [ -s "$POOL_LOGS/jobs.tsv" ] || printf '%s\n' "$JOBS_HEADER" >"$POOL_LOGS/jobs.tsv"
  GITLAB_PIN=''
  REACH_WORDS=''
  GITLAB_NOTE=''
  JOBS_CURSOR=$(now)
  if ! machine_fits; then
    hold "$FIT_WORDS"
    return 1
  fi
  say "pool $POOL_NAME for $URL: ${#R_NAME[@]} registrations, $FIT_WORDS"
  [ -z "$OVERCOMMIT_NOTE" ] || say "$OVERCOMMIT_NOTE"
  [ "$PRIVILEGED" != true ] || say "WARNING: privileged is on: a job's container can take over this machine, read every token of the pool and reach whatever the machine reaches"

  # Nothing starts before every registration has its token: a pool that ran some of them would look whole.
  local waited=0 missing unused i
  while ! stopping; do
    missing=$(missing_secrets)
    [ -n "$missing" ] || break
    if [ $((waited % 60)) = 0 ]; then
      say "waiting for secrets: $missing. Set each with service_secret_set, then run instance_start"
    fi
    nap 5
    waited=$((waited + 5))
  done
  ! stopping || return 0
  unused=''
  for ((i = ${#R_NAME[@]} + 1; i <= MAX_RUNNERS; i++)); do
    [ ! -s "$(token_file "$i")" ] || unused="${unused:+$unused, }GITLAB_RUNNER_TOKEN_$i"
  done
  [ -z "$unused" ] || say "set and not used, since runners has ${#R_NAME[@]} entries: $unused"

  prepare_images || return 0
  # A stop is read after every wait and every pause, before anything else is begun.
  ! stopping || return 0
  clean_leftovers
  if [ "$REACH" = tailnet ]; then
    tailnet_wait || return 0
  fi

  # GitLab Runner is not started before GitLab has answered for the tokens: what stands between the machine and
  # GitLab is said here in one sentence, not left to GitLab Runner's own output.
  local pause=0 verdict secrets
  while ! stopping; do
    secrets=$(secrets_tag)
    check_tokens
    verdict=$?
    ! stopping || return 0
    if [ "$verdict" = 0 ] && [ "$(accepted)" -gt 0 ]; then break; fi
    pause=$((pause == 0 ? 5 : pause * 2))
    [ "$pause" -gt "$POOL_BACKOFF_MAX" ] && pause=$POOL_BACKOFF_MAX
    if [ "$verdict" = 2 ]; then
      printf x >>"$POOL_RUN/failures"
      say "$CHECK_WORDS. Trying again in $pause seconds"
    else
      say "GitLab takes none of the pool's tokens, so no job can come. Asking again in $pause seconds"
    fi
    nap "$pause"
  done
  ! stopping || return 0
  if ! write_config; then
    hold "the pool does not start: GitLab Runner's configuration could not be written to $POOL_CONFIG"
    return 1
  fi
  say "GitLab takes the tokens of $(accepted) of ${#R_NAME[@]} registrations; starting GitLab Runner"
  start_runner || say "GitLab Runner's container could not be started; it is tried again"

  local at state last_status=0 last_verify fresh restart_at=0 restart_pause=0 written next_check=0 note
  last_verify=$(now)
  written="$secrets ${R_STATE[*]}"
  while ! stopping; do
    at=$(now)
    if [ "$REACH" = tailnet ]; then
      state=$(container_state "$TAILNET")
      if [ "${state%% *}" != true ] || [ "$(sha256sum <"$POOL_SECRETS/TAILSCALE_AUTHKEY" 2>/dev/null | cut -c1-32)" != "$TAILNET_KEY" ]; then
        say "the tailnet node is started again: it stopped, or TAILSCALE_AUTHKEY changed"
        tailnet_wait || break
        # GitLab's address on the tailnet may be another now: GitLab Runner and the jobs are told again.
        write_config && start_runner
      fi
    fi
    state=$(container_state "$RUNNER")
    if [ "${state%% *}" != true ] && [ "$at" -ge "$restart_at" ]; then
      read_jobs
      restart_pause=$((restart_pause == 0 ? 5 : restart_pause * 2))
      [ "$restart_pause" -gt "$POOL_BACKOFF_MAX" ] && restart_pause=$POOL_BACKOFF_MAX
      restart_at=$((at + restart_pause))
      printf x >>"$POOL_RUN/failures"
      say "GitLab Runner stopped (exit ${state##* }) and is started again; the jobs it ran are lost. Should it stop again, the next start is in $restart_pause seconds"
      clean_leftovers_of_jobs
      start_runner || say "GitLab Runner's container could not be started"
    elif [ "${state%% *}" = true ] && [ $((at - restart_at)) -gt 600 ]; then
      restart_pause=0
    fi
    # The tokens are asked about again when a secret changes, and every POOL_VERIFY_EVERY seconds: a token GitLab
    # stops taking is left out and said, and one that is set right comes in, without the pool being created again.
    fresh=$(secrets_tag)
    if { [ "$fresh" != "$secrets" ] && [ "$at" -ge "$next_check" ]; } || [ $((at - last_verify)) -ge "$POOL_VERIFY_EVERY" ]; then
      last_verify=$at
      if check_tokens; then
        secrets=$fresh
        [ -z "$GITLAB_NOTE" ] || say "GitLab answers again"
        GITLAB_NOTE=''
      else
        # Cut short by a stop, or failed while one came: nothing is said about GitLab, and the pool stops.
        ! stopping || break
        next_check=$((at + POOL_RECHECK))
        note="WARNING: $CHECK_WORDS. The registrations stay as they were, and GitLab is asked again"
        [ "$note" = "$GITLAB_NOTE" ] || {
          printf x >>"$POOL_RUN/failures"
          say "$note"
        }
        GITLAB_NOTE=$note
      fi
    fi
    if [ "$fresh ${R_STATE[*]}" != "$written" ] && [ "$fresh" = "$secrets" ]; then
      if write_config; then
        written="$fresh ${R_STATE[*]}"
        say "GitLab Runner's configuration was written again: $(accepted) of ${#R_NAME[@]} registrations"
      fi
    fi
    read_jobs
    if [ $((at - last_status)) -ge "$POOL_STATUS_EVERY" ]; then
      keep_disk_free
      rotate_jobs
      print_status
      last_status=$at
    fi
    nap "$POOL_TICK"
  done

  say "stopping: GitLab Runner takes no new job, and the jobs that run now get $POOL_STOP_WAIT seconds to finish before they are lost"
  docker stop --signal QUIT -t "$POOL_STOP_WAIT" "$RUNNER" >/dev/null 2>&1
  read_jobs
  docker rm -f "$RUNNER" >/dev/null 2>&1
  clean_leftovers_of_jobs
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

# The containers, volumes and networks of jobs whose GitLab Runner has gone.
clean_leftovers_of_jobs() {
  local ids
  ids=$(docker ps -aq --filter label=com.gitlab.gitlab-runner.managed=true 2>/dev/null)
  # shellcheck disable=SC2086
  [ -z "$ids" ] || docker rm -fv $ids >/dev/null 2>&1
  docker volume prune -af >/dev/null 2>&1
  docker network prune -f >/dev/null 2>&1
}

case ${1-} in
  '') install_pool ;;
  run) run_pool ;;
  status) cat "$POOL_LOGS/status.txt" 2>/dev/null || say "no status yet" ;;
  *)
    echo "usage: arca-gitlab-pool [run|status]" >&2
    exit 64
    ;;
esac
