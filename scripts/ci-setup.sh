#!/usr/bin/env bash
# Prepare a GitHub Actions runner for this repository's gate. The workflows in .github/workflows run it after the
# checkout, from the repository root:
#
#   scripts/ci-setup.sh fmt      the Rust toolchain
#   scripts/ci-setup.sh build    the toolchain and a C compiler (the build scripts of ring and SQLite need one)
#   scripts/ci-setup.sh test     the same, and the programs the tests start
#   scripts/ci-setup.sh canvas   where npm and Playwright keep their downloads, and how many test files run at once,
#                                for packages/arca-canvas-dsh
#
# It exists because a job on the runner pool starts in a small image: Ubuntu 24.04 with git, curl, jq, tar and sudo,
# and no compiler, no rustup and no Node. A GitHub-hosted runner has most of this already, and the same script then
# installs only what is missing, so both kinds of runner build with the same toolchain.
#
# The toolchain is the one rust-toolchain.toml names. rustup itself is pinned below and checked by SHA-256.
#
# Caches. The pool gives every job POOL_CACHE_DIR, a directory on the pool's own disk that the next job of the same
# slot finds again. rustup, the cargo registry, the target directory, npm's cache and the test browser go there. The
# pool empties a slot's directory when it passes that slot's share of the pool's `dependencyCache`. Without
# POOL_CACHE_DIR (a GitHub-hosted runner), or with CI_CACHE=off, everything goes under RUNNER_TEMP and ends with the
# job.
#
# Environment:
#   POOL_CACHE_DIR   set by the pool; see above
#   CI_CACHE         `off` builds from nothing even where POOL_CACHE_DIR is set
#   CI_SLOT_CPUS     the CPU ceiling of the pool class the job asked for (the class's `cpu` in the pool's settings)
#
# CPUs. A pool slot has a CPU ceiling but shows every CPU of the pool's virtual machine, so cargo, the test harness and
# Node's test runner would start one process or thread for each of those inside the slot's memory limit. The script
# therefore tells them how many CPUs the job has. It takes the number from the runner when the runner says it: the
# CPU limit of the job's own control group. Today's pool puts the limit on the slot's group, above the job's, where
# the job cannot read it, and gives the job no variable with it either; so on the pool (POOL_CACHE_DIR is set)
# CI_SLOT_CPUS is used instead. On any other runner they are told nothing and use the CPUs they see.
set -euo pipefail

# To move the pin, change the version and the SHA-256 together. The SHA-256 is the one the Rust project publishes
# beside the file (the same address with `.sha256` added).
RUSTUP_VERSION="1.29.3"
RUSTUP_INIT_SHA256="b1c8cb14e30889e7e2999e6b69e62d9e9b9ffc0f4df8180564d306d9680e6c55"
RUSTUP_INIT_URL="https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init"

say() { printf 'ci-setup: %s\n' "$*"; }
die() {
  printf 'ci-setup: %s\n' "$*" >&2
  exit 1
}

mode=${1:-}
case $mode in
  fmt | build | test | canvas) ;;
  *) die "usage: scripts/ci-setup.sh fmt|build|test|canvas" ;;
esac
[ -n "${RUNNER_TEMP:-}" ] && [ -n "${GITHUB_ENV:-}" ] && [ -n "${GITHUB_PATH:-}" ] ||
  die "this runs inside a GitHub Actions job (RUNNER_TEMP, GITHUB_ENV and GITHUB_PATH are not set)"
[ "$(uname -s)-$(uname -m)" = "Linux-x86_64" ] || die "only x86_64 Linux runners are set up by this script"
case ${CI_SLOT_CPUS:-} in
  '') ;;
  *[!0-9]* | 0*) die "CI_SLOT_CPUS is a whole number of CPUs, not '$CI_SLOT_CPUS'" ;;
esac

if [ "${CI_CACHE:-on}" != off ] && [ -n "${POOL_CACHE_DIR:-}" ] && [ -d "$POOL_CACHE_DIR" ] &&
  [ -w "$POOL_CACHE_DIR" ]; then
  cache=$POOL_CACHE_DIR
  say "caches are in $cache, which the pool keeps for the next job of this slot"
else
  cache=$RUNNER_TEMP/ci-cache
  say "caches are in $cache and end with this job"
fi
mkdir -p "$cache"

# The CPU limit of this job's own control group, rounded up to whole CPUs. Fails when there is none to read.
group_cpus() {
  local quota period
  read -r quota period 2>/dev/null </sys/fs/cgroup/cpu.max || return 1
  case $quota in '' | *[!0-9]*) return 1 ;; esac
  case $period in '' | 0 | *[!0-9]*) return 1 ;; esac
  echo $(((quota + period - 1) / period))
}

# Prints the CPUs to tell cargo and the tests, or nothing when they should use what they see.
job_cpus() {
  local cpus
  if cpus=$(group_cpus); then
    say "the runner limits this job to $cpus CPUs (its control group)" >&2
  elif [ -n "${POOL_CACHE_DIR:-}" ] && [ -n "${CI_SLOT_CPUS:-}" ]; then
    cpus=$CI_SLOT_CPUS
    say "the pool does not tell the job its CPUs; using the class's $cpus (CI_SLOT_CPUS)" >&2
  else
    return 0
  fi
  [ "$cpus" -ge 1 ] && [ "$cpus" -lt "$(nproc)" ] && echo "$cpus"
  return 0
}

# Installs the packages of the programs that are missing. Arguments: program, then its package or packages.
wanted=()
need() {
  local program=$1
  shift
  type -P "$program" >/dev/null || wanted+=("$@")
}
install_wanted() {
  [ "${#wanted[@]}" -gt 0 ] || return 0
  say "installing ${wanted[*]}"
  sudo env DEBIAN_FRONTEND=noninteractive apt-get update -q
  sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends "${wanted[@]}"
}

rustup_init() {
  local file=$RUNNER_TEMP/rustup-init
  say "installing rustup $RUSTUP_VERSION"
  curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$file" "$RUSTUP_INIT_URL"
  printf '%s  %s\n' "$RUSTUP_INIT_SHA256" "$file" | sha256sum -c - >/dev/null ||
    die "rustup-init does not have the pinned SHA-256"
  chmod 755 "$file"
  "$file" -y -q --no-modify-path --profile minimal --default-toolchain none
  rm -f "$file"
}

# With no toolchain named, rustup installs the one rust-toolchain.toml names, with its components. It would also
# replace itself with the newest rustup, which the pin above forbids.
toolchain() {
  rustup toolchain install --no-self-update >/dev/null && cargo --version >/dev/null &&
    cargo fmt --version >/dev/null && cargo clippy --version >/dev/null
}

setup_rust() {
  export RUSTUP_HOME=$cache/rustup CARGO_HOME=$cache/cargo
  export PATH=$CARGO_HOME/bin:$PATH
  local cpus
  cpus=$(job_cpus)
  if [ -d "$cache/target" ]; then state=warm; else state=cold; fi
  if [ "$(rustup --version 2>/dev/null | awk 'NR == 1 { print $2 }')" != "$RUSTUP_VERSION" ] ||
    [ "$(type -P rustup)" != "$CARGO_HOME/bin/rustup" ]; then
    rustup_init
  fi
  if ! toolchain; then
    # A job that was cancelled while it installed can leave half a toolchain in a kept cache. Start it again.
    say "the toolchain in $RUSTUP_HOME does not run; installing it again"
    rm -rf "$RUSTUP_HOME"
    rustup_init
    toolchain || die "the toolchain of rust-toolchain.toml could not be installed"
  fi
  {
    echo "RUSTUP_HOME=$RUSTUP_HOME"
    echo "CARGO_HOME=$CARGO_HOME"
    echo "CARGO_TARGET_DIR=$cache/target"
    # A checkout is new every time, so incremental data is never reused; without it the target is about half the size.
    echo "CARGO_INCREMENTAL=0"
    echo "CI_CACHE_STATE=$state"
    if [ -n "$cpus" ]; then
      echo "CARGO_BUILD_JOBS=$cpus"
      echo "RUST_TEST_THREADS=$cpus"
    fi
  } >>"$GITHUB_ENV"
  echo "$CARGO_HOME/bin" >>"$GITHUB_PATH"
  say "$(rustc --version), $(cargo --version), rustup $RUSTUP_VERSION; the cache was $state"
  say "cargo and the tests use ${cpus:-every CPU they see ($(nproc))}${cpus:+ of the $(nproc) CPUs the job sees}"
}

case $mode in
  fmt)
    setup_rust
    ;;
  build | test)
    need cc gcc libc6-dev
    if [ "$mode" = test ]; then
      # What the tests start: ssh-keygen, openssl, setfattr and getfattr, kill, tar and mkfifo, and for the runner
      # pool's own tests (crates/arca/tests/runner_pool.rs) curl, jq and flock.
      need ssh-keygen openssh-client
      need openssl openssl
      need curl curl
      need jq jq
      need flock util-linux
      need setfattr attr
      need kill procps
      need tar tar
      need mkfifo coreutils
    fi
    install_wanted
    setup_rust
    ;;
  canvas)
    cpus=$(job_cpus)
    {
      echo "npm_config_cache=$cache/npm"
      echo "PLAYWRIGHT_BROWSERS_PATH=$cache/ms-playwright"
      if [ -n "$cpus" ]; then echo "CANVAS_TEST_CONCURRENCY=$cpus"; fi
    } >>"$GITHUB_ENV"
    say "node runs ${cpus:-as many test files at once as it sees CPUs ($(nproc))}${cpus:+ test files at once}"
    ;;
esac

# What the caches hold now, to read beside the pool's share for one slot.
du -sh "$cache"/* 2>/dev/null | sed 's/^/ci-setup: cache /' || true
