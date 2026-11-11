#!/bin/bash
# A stand-in for curl, for crates/arca/tests/install_script.rs. It writes every call's arguments to
# $FAKE_DIR/curl.log and reaches no network.
#
# A call with --unix-socket plays Incus: it succeeds, unless $FAKE_DIR/incus-down is there, when it fails as curl
# does when nothing listens (7). Any other call is a download: the address must be https and under
# https://github.com/automagik-si/arca/releases/download/, and the file served is the one of that name under
# $FAKE_DIR/release (`v<version>/<file>`). An address with no such file fails as `curl -f` does on a 404 (22), which
# is what GitHub's "latest release" address answers while every release is a pre-release.
# The log holds each call's flags as they were given, so a test can read which ones a download was made with.
#
# The installer runs on a PATH of its own listed programs; the stand-in finds its tools on $FAKE_PATH.
set -u
PATH=$FAKE_PATH
root=$FAKE_DIR
printf '%s\n' "$*" >>"$root/curl.log"
[ /dev/stdin -ef /dev/null ] || echo curl >>"$root/stdin-read.log"

out='' socket='' url=''
while [ $# -gt 0 ]; do
  case $1 in
    -o) out=$2; shift 2 ;;
    --unix-socket) socket=$2; shift 2 ;;
    --retry | --proto | --max-time | --connect-timeout) shift 2 ;;
    -*) shift ;;
    *) url=$1; shift ;;
  esac
done

if [ -n "$socket" ]; then
  if [ -e "$root/incus-down" ] || [ ! -S "$socket" ]; then
    echo "curl: (7) Couldn't connect to server" >&2
    exit 7
  fi
  [ -z "$out" ] || printf '{}\n' >"$out"
  exit 0
fi

prefix=https://github.com/automagik-si/arca/releases/download/
case $url in
  "$prefix"*) file=$root/release/${url#"$prefix"} ;;
  *)
    echo "curl: the fake serves only $prefix, not $url" >&2
    exit 1
    ;;
esac
if [ ! -f "$file" ] || [ -z "$out" ]; then
  echo "curl: (22) The requested URL returned error: 404" >&2
  exit 22
fi
cat "$file" >"$out"
