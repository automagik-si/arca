#!/bin/bash
# A stand-in for the arca program, for crates/arca/tests/install_script.rs: the test serves this file as the
# release's asset, with @VERSION@ replaced, so the installer downloads, checks, installs and then runs it. It writes
# every call's arguments to $FAKE_DIR/arca.log.
#
#   --version      the version line of the real program
#   token mint     what cmd_token.rs does: refuses a hash directory its group or others can write, an id that
#                  exists and an output file that exists; writes the hash and the secret at mode 0400; prints the id
#                  and the scope, never the secret. The secret is $FAKE_ARCA_SECRET, so the test can look for it.
#   call           answers `arca_status` while $FAKE_DIR/node-running is there (the fake systemctl makes it when the
#                  unit starts), with the mode in $FAKE_DIR/node-mode (watch-only when there is no such file) and
#                  `"incusReachable":false` while $FAKE_DIR/node-cannot-reach-incus is there; it
#                  fails when no node runs, when the config is missing and when the token file is not private.
#   serve          the installer must never start the node in the foreground: recorded, and it fails.
#
# The installer runs on a PATH of its own listed programs; the stand-in finds its tools on $FAKE_PATH.
set -u
PATH=$FAKE_PATH
root=$FAKE_DIR
version=@VERSION@
printf '%s\n' "$*" >>"$root/arca.log"
[ /dev/stdin -ef /dev/null ] || echo arca >>"$root/stdin-read.log"

fail() {
  echo "arca: $*" >&2
  exit 1
}

# The value that follows a flag among the remaining arguments.
flag() {
  local want=$1
  shift
  while [ $# -gt 1 ]; do
    if [ "$1" = "$want" ]; then
      printf '%s\n' "$2"
      return 0
    fi
    shift
  done
  return 1
}

case "${1-} ${2-}" in
  "--version "*)
    echo "Automagik Arca $version (fake000, x86_64-unknown-linux-gnu)"
    ;;
  "token mint")
    scope=$(flag --scope "$@") || fail "usage"
    id=$(flag --id "$@") || fail "usage"
    out=$(flag --out "$@") || fail "usage"
    if ! dir=$(flag --hash-dir "$@"); then
      config=$(flag --config "$@") || fail "usage"
      [ -f "$config" ] || fail "the config file cannot be read"
      dir=$(dirname "$config")/tokens
    fi
    [ -d "$dir" ] && [ ! -L "$dir" ] || fail "the hash directory does not exist"
    mode=$(stat -c %a "$dir")
    case $mode in
      *[2367]? | *[2367]) fail "the hash directory is writable by its group or by others; use mode 0700 or 0500" ;;
    esac
    [ ! -e "$out" ] || fail "the output file already exists; choose a new path"
    [ ! -e "$dir/$id.hash" ] || fail "a token with id $id already exists"
    (umask 377 && printf 'hash of %s %s\n' "$scope" "$id" >"$dir/$id.hash") || fail "cannot write the hash file"
    (umask 377 && printf '%s\n' "$FAKE_ARCA_SECRET" >"$out") || fail "cannot write the secret file"
    printf '{"id":"%s","scope":"%s"}\n' "$id" "$scope"
    ;;
  "call arca_status")
    config=$(flag --config "$@") || fail "usage"
    token=$(flag --token-file "$@") || fail "usage"
    [ -f "$config" ] || fail "the config file cannot be read"
    [ -f "$token" ] || fail "the token file must be a regular file with no group or other permission bit"
    case $(stat -c %a "$token") in
      ?00) ;;
      *) fail "the token file must be a regular file with no group or other permission bit" ;;
    esac
    [ -e "$root/node-running" ] || fail "the node does not answer on its socket"
    mode=watch-only
    [ ! -f "$root/node-mode" ] || mode=$(cat "$root/node-mode")
    reach=true
    [ ! -e "$root/node-cannot-reach-incus" ] || reach=false
    printf '{"appliedSeq":0,"incusReachable":%s,"metricsSource":"incus","metricsSourceReason":null,"mode":"%s","node":"scratch","uptimeS":1,"version":"%s"}\n' \
      "$reach" "$mode" "$version"
    ;;
  "serve "*)
    fail "the installer started the node in the foreground"
    ;;
  *)
    fail "the fake does not know: $*"
    ;;
esac
