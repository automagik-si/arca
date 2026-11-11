#!/bin/bash
# A stand-in for the docker command, for crates/arca/tests/automagik_workspace.rs. It keeps each container as a
# directory under $FAKE_DOCKER_DIR/containers and writes every call's arguments to $FAKE_DOCKER_DIR/calls.log. A
# container is running while its directory holds the file `running`; the test stops one by removing that file and
# writing `exit`. A container that joins another's network namespace starts only while that one runs, as with Docker.
# A container given a directory at /run/arca-tunnel records the digest of the token file it finds there, as proof
# that the token reached it as a file. `docker exec` plays the image's health check: it prints the code in
# $FAKE_DOCKER_DIR/health-<name> (401 when there is no such file) and succeeds on 401. Nothing here is Docker.
set -u
root=$FAKE_DOCKER_DIR
mkdir -p "$root/containers"
printf '%s\n' "$*" >>"$root/calls.log"
command=${1-}
[ $# -gt 0 ] && shift

case $command in
  run)
    name='' token_dir='' joins=''
    while [ $# -gt 0 ]; do
      case $1 in
        --name) name=$2; shift 2 ;;
        --network)
          case $2 in container:*) joins=${2#container:} ;; esac
          shift 2
          ;;
        -v)
          case $2 in *:/run/arca-tunnel:ro) token_dir=${2%:/run/arca-tunnel:ro} ;; esac
          shift 2
          ;;
        -e | --restart | --user | --cap-drop | --security-opt | --entrypoint) shift 2 ;;
        -*) shift ;;
        *) break ;;
      esac
    done
    if [ -e "$root/fail-$name" ]; then
      cat "$root/fail-$name" >&2
      exit 125
    fi
    if [ -d "$root/containers/$name" ]; then
      echo "docker: Error response from daemon: Conflict. The container name \"/$name\" is already in use" >&2
      exit 125
    fi
    if [ -n "$joins" ] && [ ! -e "$root/containers/$joins/running" ]; then
      echo "docker: Error response from daemon: cannot join network namespace of a non running container: $joins" >&2
      exit 125
    fi
    mkdir -p "$root/containers/$name"
    printf '%s\n' "${1-}" >"$root/containers/$name/image"
    : >"$root/containers/$name/log"
    [ -z "$token_dir" ] || sha256sum <"$token_dir/token" | cut -d' ' -f1 >"$root/containers/$name/token.sha256"
    : >"$root/containers/$name/running"
    echo "0123456789ab"
    ;;
  inspect)
    format=$2 name=$3
    [ -d "$root/containers/$name" ] || {
      echo "Error response from daemon: No such object: $name" >&2
      exit 1
    }
    case $format in
      '{{.State.Running}}') if [ -e "$root/containers/$name/running" ]; then echo true; else echo false; fi ;;
      '{{.State.ExitCode}}') cat "$root/containers/$name/exit" 2>/dev/null || echo 0 ;;
      *) exit 2 ;;
    esac
    ;;
  rm)
    [ "${1-}" = -f ] && shift
    status=0
    for name in "$@"; do
      if [ -d "$root/containers/$name" ]; then rm -rf "${root:?}/containers/$name"; else status=1; fi
    done
    exit "$status"
    ;;
  exec)
    [ -e "$root/containers/$1/running" ] || exit 1
    code=$(cat "$root/health-$1" 2>/dev/null || echo 401)
    echo "$code"
    [ "$code" = 401 ]
    ;;
  logs)
    cat "$root/containers/$1/log" 2>/dev/null
    ;;
  *)
    echo "fake docker: unknown command $command" >&2
    exit 2
    ;;
esac
