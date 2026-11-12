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
#                  `"incusReachable":false` while $FAKE_DIR/node-cannot-reach-incus is there, and the loopback door
#                  closed, as a node whose port another program holds reports it, while $FAKE_DIR/node-door-closed is; it
#                  fails when no node runs, when the config is missing and when the token file is not private.
#   --help         the usage, with the line for `init` unless $FAKE_DIR/program-without-init is there,
#                  and naming `status --security` unless $FAKE_DIR/program-without-security is there
#   init           `init --enable-changes --yes --config <file>`: it refuses in words while $FAKE_DIR/init-refuses is
#                  there; otherwise the node's mode becomes kernel, the config gains a key (so it is no longer the
#                  installer's text) and it prints the first line of the real report. It restarts nothing here.
#   templates install   the REAL program's own `templates install`, when the test names it in $FAKE_REAL_ARCA: the
#                  installer runs this on an upgrade, and what it replaces and keeps is the real program's doing.
#                  Without that variable, or while $FAKE_DIR/program-without-replaced is there, it answers as a
#                  program from before "replaced" did, with nothing kept. It fails while
#                  $FAKE_DIR/templates-install-fails is there.
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
  "--help "*)
    echo "usage: arca <command>"
    echo "  serve --config <file>"
    [ -e "$root/program-without-init" ] ||
      echo "  init --enable-changes --config <file> [--yes] [--service-cidr <a.b.c.0/24>]"
    [ -e "$root/program-without-security" ] ||
      echo "  status [--json | --security] [--config <file>] [--token-file <file>]"
    ;;
  "init --enable-changes")
    [ "${3-}" = --yes ] || fail "the installer must answer init's question itself: --yes"
    config=$(flag --config "$@") || fail "usage"
    [ -f "$config" ] || fail "the config file cannot be read"
    if [ -e "$root/init-refuses" ]; then
      echo "arca: init changed nothing: this config has no approval page and no loopback door for one" >&2
      exit 2
    fi
    sed -i 's/"droiddProjects": \[\]/"droiddProjects": ["default"], "projects": ["arcak-svc"]/' "$config"
    echo kernel >"$root/node-mode"
    echo "Arca may now make changes on this machine."
    echo ""
    echo "  Each change still waits for you: nothing runs until you approve it with your passkey."
    echo "  No passkey is trusted yet, so nothing can be approved until you enrol the first one."
    echo "    arca setup-code --config $config"
    echo "To turn it back to a node that only looks:"
    echo "  arca init --watch-only --config $config"
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
    door=
    [ ! -e "$root/node-door-closed" ] ||
      door='"loopback":{"addr":"127.0.0.1:7471","open":false,"reason":"address in use: another program holds this port"},'
    printf '{"appliedSeq":0,"incusReachable":%s,%s"metricsSource":"incus","metricsSourceReason":null,"mode":"%s","node":"scratch","uptimeS":1,"version":"%s"}\n' \
      "$reach" "$door" "$mode" "$version"
    ;;
  "templates install")
    [ ! -e "$root/templates-install-fails" ] || fail "the templates directory cannot be read"
    if [ -n "${FAKE_REAL_ARCA-}" ] && [ ! -e "$root/program-without-replaced" ]; then
      exec "$FAKE_REAL_ARCA" "$@"
    fi
    echo '{"dir":"unused","installed":[],"kept":[],"unchanged":[]}'
    ;;
  "serve "*)
    fail "the installer started the node in the foreground"
    ;;
  *)
    fail "the fake does not know: $*"
    ;;
esac
