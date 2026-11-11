#!/bin/bash
# A stand-in for the docker command, for crates/arca/tests/runner_pool.rs. It keeps each container as a directory
# under $FAKE_DOCKER_DIR, writes every call's arguments to $FAKE_DOCKER_DIR/calls.log, and plays a runner: a container
# started from the runner image reads its just-in-time configuration from the file mounted at /run/arca/jitconfig, asks
# the fake GitHub for a job with it, runs the job once and exits. When GitHub tells it to update itself first it does
# what a real runner does: it replaces its own files, which fails when its tools directory is a mount. Nothing here is
# Docker.
set -u
# The controller runs with only the programs its script declares on its PATH. This stand-in is not the controller: it
# takes the machine's whole PATH back, and does not report its own programs as the controller's.
PATH=${FAKE_PATH:-$PATH}
unset -f command_not_found_handle 2>/dev/null
root=$FAKE_DOCKER_DIR
mkdir -p "$root/containers"
printf '%s\n' "$*" >>"$root/calls.log"
command=${1-}
[ $# -gt 0 ] && shift

# The name, the slice and the image of a `docker run` or `docker create`: the image is the first argument that is
# neither an option nor an option's value.
name='' cgroup_parent='' image='' jit_file='' tools_mounted='' command_line=''
parse_run() {
  while [ $# -gt 0 ]; do
    case $1 in
      --name) name=$2; shift 2 ;;
      --cgroup-parent) cgroup_parent=$2; shift 2 ;;
      -v)
        case $2 in
          *:/run/arca/jitconfig:ro) jit_file=${2%:/run/arca/jitconfig:ro} ;;
          *:/home/runner/externals | *:/home/runner/externals:*) tools_mounted=1 ;;
        esac
        shift 2
        ;;
      -e | --label | --network) shift 2 ;;
      -d | --privileged) shift ;;
      -*) shift ;;
      *)
        image=$1
        shift
        command_line=$*
        return
        ;;
    esac
  done
}

play_runner() {
  local dir=$1 jit=$2 tools_mounted=$3 answer job seconds result
  echo "Connected to GitHub" >>"$dir/log"
  echo "Listening for Jobs" >>"$dir/log"
  while [ -d "$dir" ]; do
    answer=$(printf '%s' "$jit" | "$FAKE_REAL_CURL" -sS -m 5 -X POST --data-binary @- "$FAKE_GITHUB/_fake/poll" 2>/dev/null) || answer=''
    if [ "$(printf '%s' "$answer" | jq -r '.update // empty' 2>/dev/null)" = true ]; then
      # GitHub wants a newer runner: the runner downloads it and replaces its own bin and externals directories.
      echo "Runner update in progress, do not shutdown runner." >>"$dir/log"
      echo "Downloading 9.999.0 runner" >>"$dir/log"
      sleep 0.4
      [ -d "$dir" ] || return
      if [ -n "$tools_mounted" ]; then
        echo "mv: cannot move '/home/runner/externals' to '/home/runner/externals.old': Device or resource busy" >>"$dir/log"
        echo 1 >"$dir/exit"
        return
      fi
      if [ -n "${FAKE_UPDATE_FAILS:-}" ]; then
        echo "An error occurred: the update could not be downloaded" >>"$dir/log"
        echo 1 >"$dir/exit"
        return
      fi
      echo "Runner will exit shortly for update, should be back online within 10 seconds." >>"$dir/log"
      printf '%s' "$jit" | "$FAKE_REAL_CURL" -sS -m 5 -X POST --data-binary @- "$FAKE_GITHUB/_fake/updated" >/dev/null 2>&1
      echo "Listening for Jobs" >>"$dir/log"
      continue
    fi
    job=$(printf '%s' "$answer" | jq -r '.job // empty' 2>/dev/null)
    if [ -n "$job" ]; then
      seconds=$(printf '%s' "$answer" | jq -r '.seconds')
      result=$(printf '%s' "$answer" | jq -r '.result')
      echo "2026-01-01 00:00:00Z: Running job: $job" >>"$dir/log"
      touch "$dir/worker"
      sleep "$seconds"
      [ -d "$dir" ] || return
      rm -f "$dir/worker"
      echo "2026-01-01 00:00:01Z: Job $job completed with result: $result" >>"$dir/log"
      printf '%s' "$jit" | "$FAKE_REAL_CURL" -sS -m 5 -X POST --data-binary @- "$FAKE_GITHUB/_fake/done" >/dev/null 2>&1
      echo 0 >"$dir/exit"
      return
    fi
    if [ "$(printf '%s' "$answer" | jq -r '.gone // empty' 2>/dev/null)" = true ]; then
      echo 1 >"$dir/exit"
      return
    fi
    sleep 0.05
  done
}

case $command in
  run | create)
    parse_run "$@"
    dir="$root/containers/$name"
    [ -n "$name" ] && [ ! -e "$dir" ] || { echo "fake docker: a container named $name exists" >&2; exit 125; }
    if [ -e "$root/fail-run" ] && [ "$image" = "$(cat "$root/fail-run")" ]; then exit 125; fi
    mkdir -p "$dir"
    echo "$image" >"$dir/image"
    : >"$dir/log"
    if [ -n "$cgroup_parent" ]; then
      # What the slot's slice would report after a job: a memory peak and the CPU time used.
      cgroup="$FAKE_CGROUP/arcapool.slice/$cgroup_parent"
      mkdir -p "$cgroup"
      echo "${FAKE_MEMORY_PEAK:-1610612736}" >"$cgroup/memory.peak"
      printf 'usage_usec %s\nuser_usec 1\nsystem_usec 1\n' "${FAKE_CPU_USEC:-73400000}" >"$cgroup/cpu.stat"
    fi
    if [ "$command" = run ] && [ "$image" = "$FAKE_RUNNER_IMAGE" ] && [ "$name" != "${name%-run}" ]; then
      # The container's command reads the configuration from the mounted file, as the real one does.
      case $command_line in
        '/bin/bash -c ACTIONS_RUNNER_INPUT_JITCONFIG=$(cat /run/arca/jitconfig) && export ACTIONS_RUNNER_INPUT_JITCONFIG && exec /home/runner/run.sh') ;;
        *) echo "fake docker: the runner's command does not read its configuration" >&2; exit 125 ;;
      esac
      [ -r "$jit_file" ] || { echo "fake docker: the runner was given no configuration file" >&2; exit 125; }
      play_runner "$dir" "$(cat "$jit_file")" "$tools_mounted" </dev/null >/dev/null 2>&1 &
    fi
    echo "id-$name"
    ;;
  rm)
    for argument in "$@"; do
      case $argument in -*) continue ;; esac
      rm -rf "$root/containers/$argument" "$root/containers/${argument#id-}"
    done
    ;;
  ps)
    for dir in "$root"/containers/*/; do
      [ -d "$dir" ] && echo "id-$(basename "$dir")"
    done
    ;;
  inspect)
    for last; do :; done
    dir="$root/containers/$last"
    [ -d "$dir" ] || exit 1
    if [ -e "$dir/exit" ]; then echo "false $(cat "$dir/exit")"; else echo "true 0"; fi
    ;;
  top)
    dir="$root/containers/$1"
    [ -d "$dir" ] || exit 1
    echo "COMMAND"
    echo "/home/runner/bin/Runner.Listener run"
    [ -e "$dir/worker" ] && echo "/home/runner/bin/Runner.Worker spawnclient"
    exit 0
    ;;
  logs)
    cat "$root/containers/$1/log" 2>/dev/null
    ;;
  exec)
    [ -d "$root/containers/$1" ] || exit 1
    ;;
  image | pull)
    [ ! -e "$root/no-images" ]
    ;;
  cp)
    for last; do :; done
    mkdir -p "$last" && echo node >"$last/node"
    ;;
  *)
    exit 0
    ;;
esac
