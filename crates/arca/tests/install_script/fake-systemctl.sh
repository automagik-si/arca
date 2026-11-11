#!/bin/bash
# A stand-in for systemctl, for crates/arca/tests/install_script.rs. It writes every call's arguments to
# $FAKE_DIR/systemctl.log and knows one unit, arca.service, as a file in $HOME/.config/systemd/user.
#
# Like the real one, `--user` answers only when XDG_RUNTIME_DIR names a directory; it also refuses while
# $FAKE_DIR/no-user-manager is there, as in a session with no user manager. `enable` and `restart` need the unit
# file; `restart` needs the program and the config the unit's ExecStart names, and then the node runs: it makes
# $FAKE_DIR/node-running, which the fake arca reads, unless $FAKE_DIR/node-does-not-start is there.
# `disable --now` ends it. `is-active` says whether the unit was started.
#
# The installer runs on a PATH of its own listed programs; the stand-in finds its tools on $FAKE_PATH.
set -u
PATH=$FAKE_PATH
root=$FAKE_DIR
printf '%s\n' "$*" >>"$root/systemctl.log"
[ /dev/stdin -ef /dev/null ] || echo systemctl >>"$root/stdin-read.log"

refuse() {
  echo "systemctl: $*" >&2
  exit 1
}

[ "${1-}" = --user ] || refuse "the installer may only talk to the user manager"
shift
[ -n "${XDG_RUNTIME_DIR-}" ] && [ -d "$XDG_RUNTIME_DIR" ] ||
  refuse "Failed to connect to bus: \$XDG_RUNTIME_DIR not defined"
[ ! -e "$root/no-user-manager" ] || refuse "Failed to connect to bus: No medium found"

unit=$HOME/.config/systemd/user/arca.service
case $* in
  show-environment) echo "HOME=$HOME" ;;
  daemon-reload) ;;
  "enable arca.service")
    [ -f "$unit" ] || refuse "Unit arca.service does not exist"
    : >"$root/unit-enabled"
    ;;
  "restart arca.service")
    [ -f "$unit" ] || refuse "Unit arca.service not found"
    start=$(sed -n 's/^ExecStart=//p' "$unit")
    read -r program verb flag config <<<"$start"
    [ -x "$program" ] && [ "$verb $flag" = "serve --config" ] && [ -f "$config" ] ||
      refuse "arca.service cannot start: $start"
    : >"$root/unit-active"
    [ -e "$root/node-does-not-start" ] || : >"$root/node-running"
    ;;
  "is-active --quiet arca.service")
    [ -e "$root/unit-active" ] || exit 3
    ;;
  "disable --now arca.service")
    rm -f "$root/unit-enabled" "$root/unit-active" "$root/node-running"
    ;;
  *) refuse "the fake does not know: $*" ;;
esac
