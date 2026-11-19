#!/bin/bash
# A stand-in for the docker command, for crates/arca/tests/runner_pool.rs. It keeps each container as a directory
# under $FAKE_DOCKER_DIR, writes every call's arguments to $FAKE_DOCKER_DIR/calls.log, and plays a runner: a container
# started from the runner image reads its just-in-time configuration from the file mounted at /run/arca/jitconfig, asks
# the fake GitHub for a job with it, runs the job once and exits. When GitHub tells it to update itself first it does
# what a real runner does: it replaces its own files, which fails when its tools directory is a mount. `docker build`
# records the Dockerfile it reads and makes a local image of it; a runner is played in a container started from the
# runner image, or from a local image built FROM it. In registration-token mode a container named <pool>-s<slot>-reg
# plays config.sh: it registers a persistent runner with the token mounted at /run/arca/token and writes the runner's
# registration into the folder mounted at /run/arca/registration; a runner started with such a registration takes job
# after job and does not exit. Nothing here is Docker.
set -u
# The controller runs with only the programs its script declares on its PATH. This stand-in is not the controller: it
# takes the machine's whole PATH back, and does not report its own programs as the controller's.
PATH=${FAKE_PATH:-$PATH}
unset -f command_not_found_handle 2>/dev/null
root=$FAKE_DOCKER_DIR
mkdir -p "$root/containers" "$root/images"
# A played runner in a session of its own, when the test asks for one (FAKE_DETACH): a real container outlives the
# controller that started it, and a played runner outlives the controller's process group then. It is no call.
play_args=()
if [ "${1-}" = __play ]; then
  shift
  play_detached=1
  play_args=("$@")
fi
[ -n "${play_detached:-}" ] || printf '%s\n' "$*" >>"$root/calls.log"
command=${1-}
[ $# -gt 0 ] && shift

# The name, the slice and the image of a `docker run` or `docker create`: the image is the first argument that is
# neither an option nor an option's value.
name='' cgroup_parent='' image='' jit_file='' tools_mounted='' home_mount='' command_line='' token_file='' reg_dir=''
reg_dir_ro='' docker_store='' tool_cache='' dependency_cache='' work_mount='' network='' command_args=()
job_hook='' job_hook_mount='' completed_hook='' completed_hook_mount='' boundary_mount='' request_mount=''
parse_run() {
  while [ $# -gt 0 ]; do
    case $1 in
      --name) name=$2; shift 2 ;;
      --cgroup-parent) cgroup_parent=$2; shift 2 ;;
      -v)
        case $2 in
          *:/run/arca/jitconfig:ro) jit_file=${2%:/run/arca/jitconfig:ro} ;;
          *:/run/arca/token:ro) token_file=${2%:/run/arca/token:ro} ;;
          *:/run/arca/registration) reg_dir=${2%:/run/arca/registration} ;;
          *:/run/arca/registration:ro) reg_dir_ro=${2%:/run/arca/registration:ro} ;;
          *:/run/arca/job-started.sh:ro) job_hook_mount=${2%:/run/arca/job-started.sh:ro} ;;
          *:/run/arca/job-completed.sh:ro) completed_hook_mount=${2%:/run/arca/job-completed.sh:ro} ;;
          *:/run/arca/boundary:ro) boundary_mount=${2%:/run/arca/boundary:ro} ;;
          *:/run/arca/boundary/request:rw) request_mount=${2%:/run/arca/boundary/request:rw} ;;
          *:/home/runner/externals | *:/home/runner/externals:*) tools_mounted=1 ;;
          *:/home/runner:ro) home_mount=${2%:/home/runner:ro} ;;
          *:/home/runner) home_mount=${2%:/home/runner} ;;
          *:/home/runner/_work) work_mount=${2%:/home/runner/_work} ;;
          *:/var/lib/docker) docker_store=${2%:/var/lib/docker} ;;
          *:/opt/hostedtoolcache) tool_cache=${2%:/opt/hostedtoolcache} ;;
          *:/var/cache/pool) dependency_cache=${2%:/var/cache/pool} ;;
        esac
        shift 2
        ;;
      --network) network=$2; shift 2 ;;
      -e)
        case $2 in
          ACTIONS_RUNNER_HOOK_JOB_STARTED=*) job_hook=${2#*=} ;;
          ACTIONS_RUNNER_HOOK_JOB_COMPLETED=*) completed_hook=${2#*=} ;;
        esac
        shift 2
        ;;
      --label | --cpuset-cpus | --mount | --entrypoint) shift 2 ;;
      -d | --privileged) shift ;;
      -*) shift ;;
      *)
        image=$1
        shift
        command_line=$*
        command_args=("$@")
        return
        ;;
    esac
  done
}

# Play a job writing to the three stores, or report what the next job can read from the mounts it received.
play_cache_job() {
  local job=$1 store marker found=0 daemon_dir="$root/containers/${name%-run}-dind"
  local markers=()
  store=$(cat "$daemon_dir/store") || return 1
  for marker in "$store" "$tool_cache" "$dependency_cache"; do
    [ -n "$marker" ] && [ -d "$marker" ] || return 1
  done
  markers=("$store/images" "$store/buildkit" "$store/volumes" "$store/containers" "$store/networks"
    "$tool_cache" "$dependency_cache")
  case $job in
    *daemon*)
      # One bind mount into the daemon's writable filesystem and one detached container.
      markers+=("$daemon_dir/private-tmp" "$store/containers/detached")
      cat "$daemon_dir/id" >"$root/${name%-run}.${job%% *}-daemon" || return 1
      if [[ $job == write* ]]; then
        mkdir -p "$dir/private-tmp"
        touch "$dir/private-tmp/.job-marker" "$work_mount/.job-marker"
      fi
      ;;
  esac
  for marker in "${markers[@]}"; do
    if [[ $job == write* || $job = 'fail daemon reset' || $job = 'interrupted job' ]]; then
      mkdir -p "$marker" && printf 'residue\n' >"$marker/.job-marker" || return 1
    elif [ -f "$marker/.job-marker" ]; then
      found=$((found + 1))
    fi
  done
  if [[ $job == read* ]]; then
    echo "$found" >"$root/${name%-run}.residue"
  fi
  if [ "$job" = 'fail daemon reset' ]; then : >"$root/refuse-hook-cleanup"; fi
}

# Run the mounted hook for real with the runner's Docker socket and bind mounts modeled under the test directory.
play_job_hook() (
  local phase=$1 source=$2 hook="$dir/$1"
  export FAKE_BOUNDARY="$boundary_mount" FAKE_DAEMON="$root/containers/${name%-run}-dind"
  export FAKE_TOOL_CACHE="$tool_cache" FAKE_DEPENDENCY_CACHE="$dependency_cache" FAKE_RUNNER_DIR="$dir" FAKE_WORK="$work_mount"
  # shellcheck disable=SC2329
  docker() {
    local store kind
    [ "$1" = --host=unix:///var/run/docker.sock ] && [ "$2" = --config=/run/arca ] || return 1
    store=$(cat "$FAKE_DAEMON/store") || return 1
    shift 2
    case $* in
      'system df --format {{.TotalCount}}')
        for kind in images containers volumes buildkit; do
          if [ -f "$store/$kind/.job-marker" ]; then echo 1; else echo 0; fi
        done
        ;;
      'network ls -q --filter type=custom')
        if [ -f "$store/networks/.job-marker" ]; then echo custom-network; fi
        ;;
      *) return 1 ;;
    esac
  }
  # shellcheck disable=SC2329
  sudo() {
    [ "$*" = '-n /usr/bin/find /opt/hostedtoolcache /var/cache/pool -xdev -mindepth 1 -print -quit' ] || return 1
    command find "$FAKE_TOOL_CACHE" "$FAKE_DEPENDENCY_CACHE" -xdev -mindepth 1 -print -quit
  }
  # shellcheck disable=SC2329
  find() {
    [ "$*" = '/tmp /home/runner/_work -xdev -mindepth 1 -delete' ] || return 1
    command find "$FAKE_RUNNER_DIR/private-tmp" "$FAKE_WORK" -xdev -mindepth 1 -delete
  }
  # Only the disposable worker may be killed here. PID 1 in this test is outside the fake container.
  # shellcheck disable=SC2329
  kill() {
    if [ "$*" = '-TERM 1' ]; then return 0; fi
    [ "$*" = "-KILL $PPID" ] && builtin kill -KILL "$PPID"
  }
  export -f docker sudo find kill
  sed 's|^boundary=/run/arca/boundary$|boundary=$FAKE_BOUNDARY|' "$source" >"$hook"
  grep -Fxq 'boundary=$FAKE_BOUNDARY' "$hook" || return 1
  # The hook's parent is a disposable stand-in for Runner.Worker.
  bash -c 'bash -e "$1" & wait "$!"' fake-worker "$hook"
)

play_runner() {
  local dir=$1 jit=$2 tools_mounted=$3 persistent=${4-} answer job seconds result hook_status listening=''
  # A just-in-time runner says it listens at once. A persistent one says so once GitHub has taken its registration,
  # as the real runner does after it has made its session; one GitHub no longer has says what the real one says.
  # A test can make a persistent runner end at once, before it reaches GitHub, as one whose start fails does.
  if [ -n "$persistent" ] && [ -n "${FAKE_PERSISTENT_DIES:-}" ]; then
    echo "An error occurred: the runner could not start" >>"$dir/log"
    end_play 1
    return
  fi
  # Or GitHub refuses the first one as it makes its session, with what runner 2.338.2 says when the runner is not
  # found; the ones after it reach GitHub.
  if [ -n "$persistent" ] && [ -n "${FAKE_PERSISTENT_NOT_FOUND:-}" ] && [ ! -e "$root/not-found-said" ]; then
    : >"$root/not-found-said"
    echo "The runner no longer exists on the server. Please reconfigure the runner." >>"$dir/log"
    end_play 1
    return
  fi
  if [ -z "$persistent" ]; then
    echo "Connected to GitHub" >>"$dir/log"
    echo "Listening for Jobs" >>"$dir/log"
  fi
  while [ -d "$dir" ]; do
    answer=$(printf '%s' "$jit" | "$FAKE_REAL_CURL" -sS -m 5 -X POST --data-binary @- "$FAKE_GITHUB/_fake/poll" 2>/dev/null) || answer=''
    if [ -n "$persistent" ] && [ -z "$listening" ] && [ -n "$answer" ] &&
      [ "$(printf '%s' "$answer" | jq -r '.gone // empty' 2>/dev/null)" != true ]; then
      listening=1
      echo "√ Connected to GitHub" >>"$dir/log"
      echo "$(date -u '+%Y-%m-%d %H:%M:%S')Z: Listening for Jobs" >>"$dir/log"
    fi
    if [ "$(printf '%s' "$answer" | jq -r '.update // empty' 2>/dev/null)" = true ]; then
      # GitHub wants a newer runner: the runner downloads it and replaces its own bin and externals directories.
      echo "Runner update in progress, do not shutdown runner." >>"$dir/log"
      echo "Downloading 9.999.0 runner" >>"$dir/log"
      sleep "${FAKE_UPDATE_SECONDS:-0.4}"
      [ -d "$dir" ] || return
      if [ -n "$tools_mounted" ]; then
        echo "mv: cannot move '/home/runner/externals' to '/home/runner/externals.old': Device or resource busy" >>"$dir/log"
        end_play 1
        return
      fi
      if [ -n "${FAKE_UPDATE_FAILS:-}" ]; then
        echo "An error occurred: the update could not be downloaded" >>"$dir/log"
        end_play 1
        return
      fi
      echo "Runner will exit shortly for update, should be back online within 10 seconds." >>"$dir/log"
      echo "Runner listener exit because of updating, re-launch runner after successful update" >>"$dir/log"
      echo "Restarting runner..." >>"$dir/log"
      printf '%s' "$jit" | "$FAKE_REAL_CURL" -sS -m 5 -X POST --data-binary @- "$FAKE_GITHUB/_fake/updated" >/dev/null 2>&1
      echo "Listening for Jobs" >>"$dir/log"
      continue
    fi
    job=$(printf '%s' "$answer" | jq -r '.job // empty' 2>/dev/null)
    if [ -n "$job" ]; then
      seconds=$(printf '%s' "$answer" | jq -r '.seconds')
      result=$(printf '%s' "$answer" | jq -r '.result')
      # The runner prints the time, then the job: the pool times the job from this line.
      echo "$(date -u '+%Y-%m-%d %H:%M:%S')Z: Running job: $job" >>"$dir/log"
      touch "$dir/worker"
      hook_status=0
      if [ -n "$job_hook" ]; then
        play_job_hook job-started.sh "$job_hook_mount" >>"$dir/log" 2>&1 || hook_status=$?
      fi
      # An ordinary hook error fails a step. Steps marked always() can still run unless the job worker died.
      if [ "$hook_status" != 137 ]; then
        case $job in
          'write caches' | 'read caches' | 'write daemon residue' | 'read daemon residue' | 'fail daemon reset' | 'interrupted job')
            play_cache_job "$job" || { end_play 1; return; } ;;
        esac
        sleep "$seconds"
      fi
      if [ "$job" = 'interrupted job' ]; then
        result=Failed
      elif [ "$hook_status" = 0 ] && [ -n "$completed_hook" ]; then
        play_job_hook job-completed.sh "$completed_hook_mount" >>"$dir/log" 2>&1 || hook_status=$?
      fi
      [ "$hook_status" = 0 ] || result=Failed
      [ -d "$dir" ] || return
      rm -f "$dir/worker"
      echo "$(date -u '+%Y-%m-%d %H:%M:%S')Z: Job $job completed with result: $result" >>"$dir/log"
      printf '%s' "$jit" | "$FAKE_REAL_CURL" -sS -m 5 -X POST --data-binary @- "$FAKE_GITHUB/_fake/done" >/dev/null 2>&1
      # A persistent runner goes back to listening for its next job, unless the test makes it stop after one.
      if [ -n "$persistent" ] && [ -n "${FAKE_PERSISTENT_STOPS_AFTER_JOB:-}" ]; then
        echo "An error occurred: the runner stopped" >>"$dir/log"
        end_play 1
        return
      fi
      if [ -n "$persistent" ]; then
        echo "Listening for Jobs" >>"$dir/log"
        continue
      fi
      end_play 0
      return
    fi
    if [ "$(printf '%s' "$answer" | jq -r '.gone // empty' 2>/dev/null)" = true ]; then
      # What runner 2.338.2 says: one removed while it listens cleans up and ends with 0; one refused as it makes its
      # session ends with an error.
      if [ -n "$persistent" ] && [ -n "$listening" ]; then
        echo "$(date -u '+%Y-%m-%d %H:%M:%S')Z: The runner no longer exists on the server. Cleaning up local configuration." >>"$dir/log"
        end_play 0
        return
      fi
      if [ -n "$persistent" ]; then
        echo "Failed to create a session. The runner registration has been deleted from the server, please re-configure. Runner registrations are automatically deleted for runners that have not connected to the service recently." >>"$dir/log"
        echo "Runner listener exit with terminated error, stop the service, no retry needed." >>"$dir/log"
      fi
      end_play 1
      return
    fi
    sleep 0.05
  done
}

# A container's process ends: its exit code, and when, as Docker's State.FinishedAt says. A controller that adopts a
# runner left by an earlier one times its job to this, not to when it next looked.
end_play() {
  echo "$1" >"$dir/exit"
  date -u '+%Y-%m-%dT%H:%M:%S.%NZ' >"$dir/finished"
}

# The file that holds a local image's Dockerfile: its name with every / and : made a _.
image_file() { printf '%s/images/%s' "$root" "$(printf '%s' "$1" | tr '/:' '__')"; }

# Whether a container of `image` plays a runner: the runner image, or a local image built from it.
runner_image() {
  [ "$1" = "$FAKE_RUNNER_IMAGE" ] && return 0
  [ -e "$(image_file "$1")" ] && [ "$(head -n1 "$(image_file "$1")")" = "FROM $FAKE_RUNNER_IMAGE" ]
}

if [ -n "${play_detached:-}" ]; then
  play_runner "${play_args[@]}" </dev/null >/dev/null 2>&1
  exit 0
fi

case $command in
  run | create)
    parse_run "$@"
    dir="$root/containers/$name"
    [ -n "$name" ] && [ ! -e "$dir" ] || { echo "fake docker: a container named $name exists" >&2; exit 125; }
    if [ -e "$root/fail-run" ] && [ "$image" = "$(cat "$root/fail-run")" ]; then exit 125; fi
    mkdir -p "$dir"
    echo "$image" >"$dir/image"
    # The host directory mounted as the runner's home, when one is: a real runner and its daemon share its files.
    echo "$home_mount" >"$dir/home"
    echo "$network" >"$dir/network"
    if [ -n "$docker_store" ]; then
      printf '%s\n' "$docker_store" >"$dir/store"
      printf 'daemon-%s\n' "$BASHPID" >"$dir/id"
    fi
    : >"$dir/log"
    if [ -n "$cgroup_parent" ]; then
      # What the slot's slice would report after a job: a memory peak and the CPU time used.
      cgroup="$FAKE_CGROUP/arcapool.slice/$cgroup_parent"
      mkdir -p "$cgroup"
      echo "${FAKE_MEMORY_PEAK:-1610612736}" >"$cgroup/memory.peak"
      printf 'usage_usec %s\nuser_usec 1\nsystem_usec 1\n' "${FAKE_CPU_USEC:-73400000}" >"$cgroup/cpu.stat"
    fi
    if [ "$command" = run ] && runner_image "$image" && [ "$name" != "${name%-reg}" ]; then
      # config.sh, in a container that ends when it is done (docker run --rm, not -d): its output is the command's.
      rm -rf "$dir"
      # shellcheck disable=SC2016
      [ "${command_args[0]-}" = /bin/bash ] && [ "${command_args[1]-}" = -c ] &&
        [ "${command_args[2]-}" = 'ACTIONS_RUNNER_INPUT_TOKEN=$(cat /run/arca/token) && export ACTIONS_RUNNER_INPUT_TOKEN && cd /home/runner && ./config.sh --unattended --replace "$@" && cp .runner .credentials .credentials_rsaparams /run/arca/registration/' ] ||
        { echo "fake docker: the registration's command does not read its token" >&2; exit 125; }
      [ -r "$token_file" ] && [ -d "$reg_dir" ] || { echo "fake docker: the registration was given no token or no folder" >&2; exit 125; }
      set -- "${command_args[@]:4}"
      url='' runner_name='' group='' labels='' ephemeral=false
      while [ $# -gt 0 ]; do
        case $1 in
          --url) url=$2; shift 2 ;;
          --name) runner_name=$2; shift 2 ;;
          --runnergroup) group=$2; shift 2 ;;
          --labels) labels=$2; shift 2 ;;
          --ephemeral) ephemeral=true; shift ;;
          --work) shift 2 ;;
          *) shift ;;
        esac
      done
      echo "--------------------------------------------------------------------------------"
      echo "|        ____ _ _   _   _       _          _        _   _                      |"
      echo "# Authentication"
      body=$(jq -nc --rawfile token "$token_file" --arg url "$url" --arg name "$runner_name" --arg group "$group" \
        --arg labels "$labels" --argjson ephemeral "$ephemeral" \
        '{token: $token, url: $url, name: $name, group: $group, labels: $labels, ephemeral: $ephemeral}')
      answer=$(printf '%s' "$body" | "$FAKE_REAL_CURL" -sS -m 5 -X POST --data-binary @- "$FAKE_GITHUB/_fake/register" 2>/dev/null)
      credential=$(printf '%s' "$answer" | jq -r '.credential // empty' 2>/dev/null)
      if [ -z "$credential" ]; then
        echo "Http response code: NotFound from 'POST https://api.github.com/actions/runner-registration'"
        echo "Response status code does not indicate success: 404 (Not Found)."
        exit 1
      fi
      echo "√ Connected to GitHub"
      echo "√ Runner successfully added"
      printf '{"agentName":"%s"}\n' "$runner_name" >"$reg_dir/.runner"
      printf '%s' "$credential" >"$reg_dir/.credentials"
      echo rsa >"$reg_dir/.credentials_rsaparams"
      echo "√ Settings Saved."
      exit 0
    fi
    if [ "$command" = run ] && runner_image "$image" && [ "$name" != "${name%-run}" ] && [ -n "$reg_dir_ro" ]; then
      # A persistent runner reads the registration a registration container wrote.
      if [ -n "$job_hook" ]; then
        [ "$job_hook" = /run/arca/job-started.sh ] && [ -x "$job_hook_mount" ] || exit 125
        [ "$completed_hook" = /run/arca/job-completed.sh ] && [ -x "$completed_hook_mount" ] || exit 125
        [ -n "$boundary_mount" ] && [ "$request_mount" = "$boundary_mount/request" ] || exit 125
        mkdir -p "$dir/private-tmp"
      fi
      case $command_line in
        '/bin/bash -c cp /run/arca/registration/.runner /run/arca/registration/.credentials /run/arca/registration/.credentials_rsaparams /home/runner/ && exec /home/runner/run.sh') ;;
        *) echo "fake docker: the persistent runner's command does not read its registration" >&2; exit 125 ;;
      esac
      [ -s "$reg_dir_ro/.runner" ] && [ -s "$reg_dir_ro/.credentials_rsaparams" ] && [ -r "$reg_dir_ro/.credentials" ] ||
        { echo "fake docker: the runner was given no registration" >&2; exit 125; }
      if [ -n "${FAKE_DETACH:-}" ]; then
        setsid "$0" __play "$dir" "$(cat "$reg_dir_ro/.credentials")" "$tools_mounted" persistent </dev/null >/dev/null 2>&1 &
      else
        play_runner "$dir" "$(cat "$reg_dir_ro/.credentials")" "$tools_mounted" persistent </dev/null >/dev/null 2>&1 &
      fi
      echo "id-$name"
      exit 0
    fi
    if [ "$command" = run ] && runner_image "$image" && [ "$name" != "${name%-run}" ]; then
      # The container's command reads the configuration from the mounted file, as the real one does.
      case $command_line in
        '/bin/bash -c ACTIONS_RUNNER_INPUT_JITCONFIG=$(cat /run/arca/jitconfig) && export ACTIONS_RUNNER_INPUT_JITCONFIG && exec /home/runner/run.sh') ;;
        *) echo "fake docker: the runner's command does not read its configuration" >&2; exit 125 ;;
      esac
      [ -r "$jit_file" ] || { echo "fake docker: the runner was given no configuration file" >&2; exit 125; }
      if [ -n "${FAKE_DETACH:-}" ]; then
        setsid "$0" __play "$dir" "$(cat "$jit_file")" "$tools_mounted" </dev/null >/dev/null 2>&1 &
      else
        play_runner "$dir" "$(cat "$jit_file")" "$tools_mounted" </dev/null >/dev/null 2>&1 &
      fi
    fi
    if [ -n "$docker_store" ]; then cat "$dir/id"; else echo "id-$name"; fi
    ;;
  rm)
    # Removing a container takes time on a real machine: a test can say how long.
    [ -z "${FAKE_RM_SECONDS:-}" ] || sleep "$FAKE_RM_SECONDS"
    status=0
    for argument in "$@"; do
      case $argument in -*) continue ;; esac
      name=${argument#id-}
      case $name in
        *-dind)
          if [ "$(cat "$root/containers/${name%-dind}-run/network" 2>/dev/null)" = "container:$name" ]; then
            status=1
            continue
          fi
          ;;
      esac
      if [[ $name == *-dind && -d $root/containers/$name && -e $root/refuse-hook-cleanup ]]; then
        status=1
        continue
      fi
      rm -rf "$root/containers/$argument" "$root/containers/$name"
    done
    exit "$status"
    ;;
  ps)
    for dir in "$root"/containers/*/; do
      [ -d "$dir" ] || continue
      case " $* " in
        *' {{.ID}} {{.Names}} '*) echo "id-$(basename "$dir") $(basename "$dir")" ;;
        *' {{.Names}} '*) basename "$dir" ;;
        *) echo "id-$(basename "$dir")" ;;
      esac
    done
    ;;
  inspect)
    for last; do :; done
    dir="$root/containers/$last"
    [ -d "$dir" ] || exit 1
    case " $* " in
      *'{{.Id}} {{.State.Running}}'*)
        [ ! -e "$dir/exit" ] && printf '%s true\n' "$(cat "$dir/id")"
        ;;
      *FinishedAt*)
        # An ended container says when: a running one, or one whose end was never played, has no time to give.
        if [ -e "$dir/finished" ]; then cat "$dir/finished"; else echo "0001-01-01T00:00:00Z"; fi
        ;;
      *) if [ -e "$dir/exit" ]; then echo "false $(cat "$dir/exit")"; else echo "true 0"; fi ;;
    esac
    ;;
  top)
    dir="$root/containers/$1"
    [ -d "$dir" ] || exit 1
    # Docker finds a container's processes by their ids, so it refuses ps options that leave the id out.
    case " $* " in
      *pid* | *" -ef "* | *" aux "*) ;;
      *)
        [ $# -le 1 ] || { echo "Error response from daemon: Couldn't find PID field in ps output" >&2; exit 1; }
        ;;
    esac
    echo "COMMAND"
    echo "/home/runner/bin/Runner.Listener run"
    [ -e "$dir/worker" ] && echo "/home/runner/bin/Runner.Worker spawnclient"
    exit 0
    ;;
  logs)
    # Each read is one look of the slot at its runner: a test can wait for one.
    [ ! -d "$root/containers/$1" ] || printf x >>"$root/containers/$1/looks"
    cat "$root/containers/$1/log" 2>/dev/null
    ;;
  exec)
    dir="$root/containers/$1"
    [ -d "$dir" ] || exit 1
    if [ -f "$dir/store" ]; then
      store=$(cat "$dir/store")
      case ${4-} in
        'ids=$(docker ps -aq); [ -z "$ids" ] || docker rm -fv $ids')
          [ ! -e "$root/refuse-hook-cleanup" ] && rm -rf "$store/containers"
          ;;
        *'docker volume prune -af'*) rm -f "$store/"{volumes,containers,networks}/.job-marker ;;
        *'docker system prune -af'*) rm -f "$store/images/.job-marker" "$store/buildkit/.job-marker" ;;
      esac
    fi
    ;;
  image)
    # `image inspect`: an image named by digest is there unless the test says none is; a local name only once built.
    for last; do :; done
    case $last in
      *@sha256:*) [ ! -e "$root/no-images" ] ;;
      *) [ -e "$(image_file "$last")" ] ;;
    esac
    ;;
  pull)
    [ ! -e "$root/no-images" ]
    ;;
  build)
    # `docker build -t <name> -`: the Dockerfile comes on the standard input. A test makes it fail with the package
    # manager's words in $root/fail-build. Without buildx, Docker's legacy builder keeps a failed step's container
    # unless --force-rm is given: that container has no label of the pool, so it is modeled here as it is left.
    tag='' force_rm=''
    while [ $# -gt 0 ]; do
      case $1 in
        -t) tag=$2; shift 2 ;;
        --force-rm) force_rm=1; shift ;;
        *) shift ;;
      esac
    done
    recipe=$(cat)
    printf '%s\n' "$recipe" >>"$root/dockerfiles.log"
    if [ -e "$root/fail-build" ]; then
      if [ -z "$force_rm" ]; then
        mkdir -p "$root/containers/build-failed-$$"
      fi
      echo "Step 3/4 : RUN apt-get update && apt-get install ..."
      cat "$root/fail-build"
      echo "The command '/bin/sh -c apt-get update' returned a non-zero code: 100" >&2
      exit 1
    fi
    [ -n "$tag" ] || exit 1
    printf '%s\n' "$recipe" >"$(image_file "$tag")"
    echo "Successfully tagged $tag"
    ;;
  cp)
    for last; do :; done
    mkdir -p "$last" && echo node >"$last/node"
    ;;
  *)
    exit 0
    ;;
esac
