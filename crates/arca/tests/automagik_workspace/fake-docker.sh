#!/bin/bash
# A stand-in for the docker command, for crates/arca/tests/automagik_workspace.rs. It writes every call's arguments
# to $FAKE_DOCKER_DIR/calls.log. Nothing here is Docker.
#
# - A container is a directory under $FAKE_DOCKER_DIR/containers. It is running while it holds the file `running`;
#   the test stops one by removing that file and writing `exit`. It records the network it was made on (`netmode`:
#   what --network said, `default` when nothing did). One that joins another's network namespace starts only while
#   that one runs, as with Docker.
# - A container given a directory at /run/arca-tunnel or /run/arca-tailnet records the digest of the token or auth
#   key file it finds there, as proof that the secret reached it as a file.
# - `docker network` keeps each network as a directory under $FAKE_DOCKER_DIR/networks. A running container that is
#   connected to one gets the next address of 10.88.0.0/24 there, which `docker inspect` then answers.
# - While $FAKE_DOCKER_DIR/fail-inspect is there, `docker inspect` fails as it does when the daemon cannot be
#   reached; while warn-inspect is there it prints a warning beside its answer.
# - `docker exec <name> tailscale ... status --json` prints $FAKE_DOCKER_DIR/tailnet-status.json (a node that is
#   still starting when there is no such file). Any other `docker exec` plays the image's health check: it prints
#   the code in $FAKE_DOCKER_DIR/health-<name> (401 when there is no such file) and succeeds on 401.
set -u
root=$FAKE_DOCKER_DIR
mkdir -p "$root/containers" "$root/networks"
printf '%s\n' "$*" >>"$root/calls.log"
command=${1-}
[ $# -gt 0 ] && shift

case $command in
  run)
    name='' token_dir='' token_file='' joins='' netmode=default
    while [ $# -gt 0 ]; do
      case $1 in
        --name) name=$2; shift 2 ;;
        --network)
          netmode=$2
          case $2 in container:*) joins=${2#container:} ;; esac
          shift 2
          ;;
        -v)
          case $2 in
            *:/run/arca-tunnel:ro) token_dir=${2%:/run/arca-tunnel:ro} token_file=token ;;
            *:/run/arca-tailnet:ro) token_dir=${2%:/run/arca-tailnet:ro} token_file=authkey ;;
          esac
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
    printf '%s\n' "$netmode" >"$root/containers/$name/netmode"
    : >"$root/containers/$name/log"
    [ -z "$token_dir" ] || sha256sum <"$token_dir/$token_file" | cut -d' ' -f1 >"$root/containers/$name/token.sha256"
    : >"$root/containers/$name/running"
    echo "0123456789ab"
    ;;
  inspect)
    [ "${1-}" = --type ] && shift 2
    format=$2 name=$3
    # A warning beside a good answer, as a Docker client prints one on its standard error.
    [ ! -e "$root/warn-inspect" ] || echo "WARNING: Error loading config file: permission denied" >&2
    if [ -e "$root/fail-inspect" ]; then
      echo "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?" >&2
      exit 1
    fi
    [ -d "$root/containers/$name" ] || {
      echo "Error: No such object: $name" >&2
      exit 1
    }
    case $format in
      '{{.State.Running}} {{.State.ExitCode}}')
        if [ -e "$root/containers/$name/running" ]; then
          echo "true 0"
        else
          echo "false $(cat "$root/containers/$name/exit" 2>/dev/null || echo 0)"
        fi
        ;;
      '{{.HostConfig.NetworkMode}}') cat "$root/containers/$name/netmode" ;;
      '{{with index .NetworkSettings.Networks "'*'"}}{{.IPAddress}}{{end}}')
        network=${format#*Networks \"}
        cat "$root/containers/$name/address-${network%%\"*}" 2>/dev/null || echo
        ;;
      *) exit 2 ;;
    esac
    ;;
  network)
    action=${1-}
    shift
    case $action in
      inspect)
        [ -d "$root/networks/$1" ] || {
          echo "Error response from daemon: network $1 not found" >&2
          exit 1
        }
        ;;
      create)
        [ "${1-}" = --internal ] && shift
        if [ -e "$root/fail-network" ]; then
          cat "$root/fail-network" >&2
          exit 1
        fi
        mkdir "$root/networks/$1"
        ;;
      connect)
        [ -d "$root/networks/$1" ] && [ -e "$root/containers/$2/running" ] || {
          echo "Error response from daemon: no such network or container" >&2
          exit 1
        }
        printf '%s\n' "$2" >>"$root/networks/$1/members"
        # Counted with bash alone: this script runs with the keeper's own short list of programs.
        mapfile -t members <"$root/networks/$1/members"
        echo "10.88.0.$((${#members[@]} + 1))" >"$root/containers/$2/address-$1"
        ;;
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
    if [ "${2-}" = tailscale ]; then
      cat "$root/tailnet-status.json" 2>/dev/null || echo '{"BackendState":"Starting","Self":{"DNSName":""}}'
      exit 0
    fi
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
