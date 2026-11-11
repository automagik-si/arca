#!/bin/bash
# A stand-in for the docker command, for crates/arca/tests/gitlab_runner_pool.rs. It keeps each container as a
# directory under $FAKE_DOCKER_DIR with what it was started with (its labels, mounts, environment and the whole
# argument line), and writes every call's arguments to $FAKE_DOCKER_DIR/calls.log. It plays no GitLab Runner: the test
# writes the runner's output lines and makes the jobs' containers itself, as directories with a `job` file. Nothing
# here is Docker.
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

case $command in
  run)
    name='' image='' labels='' mounts='' config=''
    line=$*
    while [ $# -gt 0 ]; do
      case $1 in
        --name) name=$2; shift 2 ;;
        --label) labels="$labels$2"$'\n'; shift 2 ;;
        -v)
          mounts="$mounts$2"$'\n'
          case $2 in *:/etc/gitlab-runner) config=${2%:/etc/gitlab-runner} ;; esac
          shift 2
          ;;
        -e | --network | --restart | --cap-drop | --cap-add | --device | --add-host) shift 2 ;;
        -*) shift ;;
        *) image=$1; break ;;
      esac
    done
    dir="$root/containers/$name"
    [ -n "$name" ] && [ ! -e "$dir" ] || { echo "fake docker: a container named $name exists" >&2; exit 125; }
    if [ -e "$root/fail-run" ] && [ "$image" = "$(cat "$root/fail-run")" ]; then exit 125; fi
    mkdir -p "$dir"
    printf '%s' "$labels" >"$dir/labels"
    printf '%s' "$mounts" >"$dir/mounts"
    printf '%s\n' "$line" >"$dir/args"
    echo "$image" >"$dir/image"
    : >"$dir/log"
    # What GitLab Runner would find where its configuration is mounted: the modes of the directory and the file.
    if [ -n "$config" ]; then
      stat -c '%a' "$config" "$config/config.toml" >"$dir/modes" 2>&1
      cat "$config/.runner_system_id" >"$dir/system-id" 2>/dev/null
    fi
    printf x >>"$root/runs.$name"
    echo "id-$name"
    ;;
  rm)
    for argument in "$@"; do
      case $argument in -*) continue ;; esac
      rm -rf "$root/containers/$argument" "$root/containers/${argument#id-}"
    done
    ;;
  ps)
    [ ! -e "$root/ps-fails" ] || exit 1
    filter='' format=''
    while [ $# -gt 0 ]; do
      case $1 in
        --filter) filter=${2#label=}; shift 2 ;;
        --format) format=$2; shift 2 ;;
        *) shift ;;
      esac
    done
    for dir in "$root"/containers/*/; do
      [ -d "$dir" ] || continue
      [ -z "$filter" ] || grep -qxF "$filter" "$dir/labels" 2>/dev/null || continue
      if [ -n "$format" ]; then cat "$dir/job" 2>/dev/null; else echo "id-$(basename "$dir")"; fi
    done
    ;;
  inspect)
    for last; do :; done
    dir="$root/containers/$last"
    [ -d "$dir" ] || exit 1
    if [ -e "$dir/exit" ]; then echo "false $(cat "$dir/exit")"; else echo "true 0"; fi
    ;;
  logs)
    for last; do :; done
    cat "$root/containers/$last/log" 2>/dev/null
    ;;
  stop)
    for last; do :; done
    [ -d "$root/containers/$last" ] && echo 0 >"$root/containers/$last/exit"
    ;;
  exec)
    # Only the tailnet node is asked anything, and only for its status: `tailscale --socket <path> status --json`,
    # which the test writes as the real one answers (every node with its full name, ending with a dot, and its
    # addresses). `tailscale ip <name>` is not played: the real one finds a node by its full name only with the last
    # dot, so a controller that asked it would get nothing here either.
    [ -d "$root/containers/$1" ] && [ ! -e "$root/containers/$1/exit" ] || exit 1
    shift
    [ "${1-}" = tailscale ] && [ "${2-}" = --socket ] || exit 1
    shift 3
    [ "$*" = 'status --json' ] || exit 1
    cat "$root/tailnet-status.json" 2>/dev/null || echo '{"BackendState":"Running"}'
    ;;
  image)
    case ${1-} in
      prune) touch "$root/pruned" ;;
      *) [ ! -e "$root/no-images" ] ;;
    esac
    ;;
  pull)
    [ ! -e "$root/no-images" ]
    ;;
  *)
    exit 0
    ;;
esac
