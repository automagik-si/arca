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
#
# Environment:
#   ARCA_PORTABLE_TOOLS   where the tools are kept (default: $XDG_CACHE_HOME/arca-portable-tools,
#                         or ~/.cache/arca-portable-tools)
#
# Prints the path of the binary, its SHA-256 and the highest GLIBC symbol version it needs.
# Exits non-zero when that version is above the floor, or when any step fails.
set -euo pipefail

# The glibc the binary may need at most. 2.34 is Enterprise Linux 9.
GLIBC_FLOOR="2.34"
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

[ "$(uname -s)" = "Linux" ] && [ "$(uname -m)" = "x86_64" ] ||
    fail "this script builds on x86_64 Linux only (found $(uname -s) $(uname -m))"
for tool in cargo curl sha256sum tar xz objdump; do
    command -v "$tool" >/dev/null || fail "$tool is not on PATH"
done

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tools="${ARCA_PORTABLE_TOOLS:-${XDG_CACHE_HOME:-$HOME/.cache}/arca-portable-tools}"
mkdir -p "$tools"

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

# cargo finds `cargo-zigbuild` on PATH; cargo-zigbuild is told which zig to use.
export PATH="$zigbuild_dir:$PATH"
export CARGO_ZIGBUILD_ZIG_PATH="$zig_dir/zig"

cd "$repo"
cargo zigbuild --release --locked -p arca --bin arca --target "$RUST_TARGET.$GLIBC_FLOOR"

target_dir="$(cargo metadata --format-version 1 --no-deps --locked |
    sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$target_dir" ] || fail "could not read the target directory from cargo metadata"
binary="$target_dir/$RUST_TARGET/release/arca"
[ -x "$binary" ] || fail "the build did not produce $binary"

# The highest GLIBC_x.y version among the dynamic symbols is the oldest glibc that can load
# the binary. A binary with no such symbol does not depend on glibc at all.
glibc_max="$(objdump -T "$binary" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' |
    sort -u -V | tail -n 1 || true)"
sha256="$(sha256sum "$binary" | cut -d ' ' -f 1)"

echo "binary:    $binary"
echo "sha256:    $sha256"
echo "max GLIBC: ${glibc_max:-none}"

if [ -n "$glibc_max" ] &&
    [ "$(printf '%s\n%s\n' "$glibc_max" "$GLIBC_FLOOR" | sort -V | tail -n 1)" != "$GLIBC_FLOOR" ]; then
    fail "the binary needs GLIBC $glibc_max, above the $GLIBC_FLOOR floor"
fi
