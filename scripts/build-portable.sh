#!/usr/bin/env bash
# Build a release `arca` that starts on hosts with an older glibc than the build machine's.
#
# A plain `cargo build --release` links against the build machine's glibc, so the binary needs
# that version or newer. This script links against glibc 2.34 instead (Enterprise Linux 9 and
# newer), using zig as the C compiler and linker through cargo-zigbuild.
#
# No root is needed. The two tools are fetched once as release archives, checked against the
# SHA-256 pinned below, and unpacked under the tools directory. Nothing is installed system-wide.
#
#   scripts/build-portable.sh
#   scripts/build-portable.sh --check-paths <file>   only the check for build-machine paths, on a file; builds nothing
#
# Environment:
#   ARCA_PORTABLE_TOOLS   where the tools are kept (default: $XDG_CACHE_HOME/arca-portable-tools,
#                         or ~/.cache/arca-portable-tools)
#
# Prints the zig that built the binary, the binary's path, its SHA-256 and the highest GLIBC
# symbol version it needs. Exits non-zero when that version is above the ceiling, when no
# such version can be read, or when any step fails.
#
# The binary carries no path of the build machine: cargo's home (its registry sources and git
# checkouts), the toolchain's own directory and this checkout are compiled under fixed names
# (/cargo, /rustc/sysroot and /arca), and the script fails when a path under the home directory,
# cargo's home, the checkout or the build's own directories is still found in the binary. It
# prints the paths it found. A string of the files the binary is built from is not such a path,
# even where it starts like one: a template that names /home/runner/_work inside a container
# says so on every machine, also on one whose home directory is /home/runner.
set -euo pipefail

# The glibc the binary may need at most. 2.34 is Enterprise Linux 9.
GLIBC_CEILING="2.34"
RUST_TARGET="x86_64-unknown-linux-gnu"

# Pinned tools. To move a pin, change the version and the SHA-256 together; the SHA-256 values
# are the ones the projects publish (ziglang.org/download/index.json, and the release's
# sha256.sum on github.com/rust-cross/cargo-zigbuild).
ZIG_VERSION="0.17.1"
ZIG_ARCHIVE="zig-x86_64-linux-${ZIG_VERSION}.tar.xz"
ZIG_URL="https://ziglang.org/download/${ZIG_VERSION}/${ZIG_ARCHIVE}"
ZIG_SHA256="3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a"

ZIGBUILD_VERSION="0.24.1"
ZIGBUILD_ARCHIVE="cargo-zigbuild-x86_64-unknown-linux-gnu.tar.xz"
ZIGBUILD_URL="https://github.com/rust-cross/cargo-zigbuild/releases/download/v${ZIGBUILD_VERSION}/${ZIGBUILD_ARCHIVE}"
ZIGBUILD_SHA256="32581c8e13f27a807bb244aaaac870522f5b51263c766cf77b1a99771ce9fa5c"

fail() {
    echo "build-portable: $*" >&2
    exit 1
}

repo="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"

# check_paths <file> [<directory>...]: fails when <file> names a path of the build machine.
#
# A path of the build machine is a string that starts with the home directory, cargo's home, this
# checkout or one of the directories given, followed by "/" and then runs on through the
# characters a path is written with. Every such string is refused, and printed, but one that is,
# whole, such a string in a tracked file the binary is built from (SOURCES below): that is the
# product's own text (a template naming a path inside a container, say), which the binary carries
# on every machine. A leaked path names a file under one of those directories, and equals no text
# of ours. A test, a document or a script is not what the binary is built from, so a path written
# in one of them excuses nothing. Outside a git checkout nothing is known to be the product's
# text, and every such string is refused.
#
# SOURCES: the tracked files whose text the binary can carry. A new directory that the binary
# embeds files from belongs here; until it is, a path-like string in it fails the build on the one
# machine whose directories it happens to name, and the build says which string.
SOURCES=(':(glob)crates/*/src/**' templates ontology wit)
check_paths() {
    local file="$1" private pattern found="" known="" leaked count
    shift
    for private in "$HOME" "$cargo_home" "$repo" "$@"; do
        private="${private%/}"
        [ -n "$private" ] || continue
        pattern="$(printf '%s' "$private" | sed 's/[][\\.*^$+?(){}|]/\\&/g')/[A-Za-z0-9_.+@:=,%~/-]*"
        found+="$(LC_ALL=C grep -a -o -E -e "$pattern" -- "$file" || true)"$'\n'
        known+="$(LC_ALL=C git -C "$repo" grep -a -h -o -E -e "$pattern" -- "${SOURCES[@]}" 2>/dev/null || true)"$'\n'
    done
    leaked="$(LC_ALL=C comm -23 <(printf '%s' "$found" | sed '/^$/d' | LC_ALL=C sort -u) \
        <(printf '%s' "$known" | sed '/^$/d' | LC_ALL=C sort -u))"
    [ -n "$leaked" ] || return 0
    count="$(printf '%s\n' "$leaked" | wc -l)"
    {
        echo "build-portable: $file names $count path(s) of the build machine (at most 20 are listed):"
        printf '%s\n' "$leaked" | head -n 20 | sed 's/^/  /'
    } >&2
    fail "the binary still names a path of the build machine"
}

if [ "${1-}" = "--check-paths" ]; then
    [ "$#" -eq 2 ] && [ -f "$2" ] || fail "usage: scripts/build-portable.sh --check-paths <file>"
    check_paths "$2"
    echo "build-portable: $2 names no path of the build machine"
    exit 0
fi
[ "$#" -eq 0 ] || fail "usage: scripts/build-portable.sh [--check-paths <file>]"

[ "$(uname -s)" = "Linux" ] && [ "$(uname -m)" = "x86_64" ] ||
    fail "this script builds on x86_64 Linux only (found $(uname -s) $(uname -m))"
for tool in cargo curl sha256sum tar xz objdump; do
    command -v "$tool" >/dev/null || fail "$tool is not on PATH"
done

tools="${ARCA_PORTABLE_TOOLS:-${XDG_CACHE_HOME:-$HOME/.cache}/arca-portable-tools}"
mkdir -p "$tools"
# Absolute, so the path still names the same directory after the cd into the repository.
tools="$(CDPATH= cd -- "$tools" && pwd)"

# fetch <url> <sha256> <directory>: download an archive, check it, unpack it into <directory>.
# The directory is renamed into place only after the check and the unpack both succeed, so an
# interrupted run never leaves a half-unpacked tool behind.
fetch() {
    local url="$1" sha="$2" dest="$3"
    [ -d "$dest" ] && return 0
    local tmp
    tmp="$(mktemp -d "$tools/.fetch.XXXXXX")"
    echo "build-portable: fetching $url" >&2
    curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
        --output "$tmp/archive.tar.xz" "$url" || { rm -rf "$tmp"; fail "download failed: $url"; }
    if ! echo "$sha  $tmp/archive.tar.xz" | sha256sum --check --status; then
        rm -rf "$tmp"
        fail "SHA-256 mismatch for $url (expected $sha)"
    fi
    mkdir "$tmp/unpacked"
    tar -xJf "$tmp/archive.tar.xz" -C "$tmp/unpacked" --strip-components=1 ||
        { rm -rf "$tmp"; fail "could not unpack $url"; }
    mv "$tmp/unpacked" "$dest"
    rm -rf "$tmp"
}

zig_dir="$tools/zig-$ZIG_VERSION"
zigbuild_dir="$tools/cargo-zigbuild-$ZIGBUILD_VERSION"
fetch "$ZIG_URL" "$ZIG_SHA256" "$zig_dir"
fetch "$ZIGBUILD_URL" "$ZIGBUILD_SHA256" "$zigbuild_dir"
[ -x "$zig_dir/zig" ] || fail "no zig in $zig_dir; remove that directory and run again"
[ -x "$zigbuild_dir/cargo-zigbuild" ] ||
    fail "no cargo-zigbuild in $zigbuild_dir; remove that directory and run again"

# Only the pinned tools may build the binary. cargo-zigbuild takes the zig named by
# CARGO_ZIGBUILD_ZIG_COMMAND before it looks for a Python `ziglang` package or a zig on PATH,
# so that variable is set to the pinned zig, and the inherited values that could name another
# zig, add arguments to it or state a version for it are dropped. The pinned cargo-zigbuild is
# run by its full path, because `cargo zigbuild` could find another one in cargo's bin directory.
unset CARGO_ZIGBUILD_ZIG_COMMAND_ARGS CARGO_ZIGBUILD_ZIG_VERSION
export CARGO_ZIGBUILD_ZIG_COMMAND="$zig_dir/zig"
export CARGO_ZIGBUILD_ZIG_PATH="$zig_dir/zig"
zig_version="$("$CARGO_ZIGBUILD_ZIG_COMMAND" version)"
[ "$zig_version" = "$ZIG_VERSION" ] ||
    fail "$CARGO_ZIGBUILD_ZIG_COMMAND is zig $zig_version, not the pinned $ZIG_VERSION"

# rustc writes the path of each source file it compiled into the binary's panic messages. The
# places those files come from are given fixed names, so the binary does not name the build
# machine's directories: the toolchain's own directory (where the standard library's sources are
# when that component is installed), cargo's home (its registry sources and its git checkouts) and
# this checkout. Flags the caller already set are kept; they go in CARGO_ENCODED_RUSTFLAGS, whose
# separator lets a path hold a space.
cd "$repo"
sysroot="$(rustc --print sysroot 2>/dev/null || true)"
sep=$'\x1f'
if [ -z "${CARGO_ENCODED_RUSTFLAGS-}" ] && [ -n "${RUSTFLAGS-}" ]; then
    read -r -a inherited <<<"$RUSTFLAGS"
    CARGO_ENCODED_RUSTFLAGS="$(IFS="$sep" && printf '%s' "${inherited[*]}")"
fi
# remap <directory> <name>: sources under <directory> are compiled as if they were under <name>.
remap() {
    CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS:+$CARGO_ENCODED_RUSTFLAGS$sep}--remap-path-prefix=$1=$2"
}
[ -z "$sysroot" ] || remap "$sysroot" /rustc/sysroot
remap "$cargo_home" /cargo
remap "$repo" /arca
export CARGO_ENCODED_RUSTFLAGS
unset RUSTFLAGS

"$zigbuild_dir/cargo-zigbuild" zigbuild --release --locked -p arca --bin arca --target "$RUST_TARGET.$GLIBC_CEILING"

metadata="$(cargo metadata --format-version 1 --no-deps --locked)"
target_dir="$(printf '%s' "$metadata" | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$target_dir" ] || fail "could not read the target directory from cargo metadata"
# Where cargo keeps what a build makes on the way, when it is not the target directory.
build_dir="$(printf '%s' "$metadata" | sed -n 's/.*"build_directory":"\([^"]*\)".*/\1/p')"
binary="$target_dir/$RUST_TARGET/release/arca"
[ -x "$binary" ] || fail "the build did not produce $binary"

# No path of the build machine may be left in the binary: none under the home directory, cargo's
# home or this checkout, and none under the directories the build itself wrote to.
check_paths "$binary" "$target_dir" "$build_dir" "$sysroot" "$tools"

# The highest GLIBC_x.y version among the dynamic symbols is the oldest glibc that can load
# the binary. objdump runs as its own command so that its failure stops the script; a binary
# that shows no such symbol was not checked, and that is a failure too.
symbols="$(objdump -T "$binary")" || fail "objdump could not read $binary"
glibc_max="$(printf '%s\n' "$symbols" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' |
    sort -u -V | tail -n 1 || true)"
sha256="$(sha256sum "$binary" | cut -d ' ' -f 1)"

echo "zig:       $CARGO_ZIGBUILD_ZIG_COMMAND ($zig_version)"
echo "binary:    $binary"
echo "sha256:    $sha256"
echo "max GLIBC: ${glibc_max:-none found}"

[ -n "$glibc_max" ] || fail "no GLIBC symbol version found in $binary, so the ceiling was not checked"
[ "$(printf '%s\n%s\n' "$glibc_max" "$GLIBC_CEILING" | sort -V | tail -n 1)" = "$GLIBC_CEILING" ] ||
    fail "the binary needs GLIBC $glibc_max, above the $GLIBC_CEILING ceiling"
