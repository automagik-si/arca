#!/usr/bin/env bash
# Build the release assets of Automagik Arca for the tag checked out here, and print the command that would publish them.
# The release workflow (.github/workflows/release.yml) builds every release with this script and publishes the files
# itself; run by hand, the script is for trying a build.
#
#   scripts/release.sh              from a clean tree whose HEAD is the tag v<version>
#   scripts/release.sh --rehearse   builds and checks the same files without the tag, to try the script before the
#                                   tag exists; it prints no publish command
#
# It publishes nothing and writes nothing to the network of its own: it does not create the tag, push, or run `gh`. The
# only downloads are the two pinned build tools that scripts/build-portable.sh fetches on its first run.
#
# What it does:
#   1. refuses a tree with uncommitted or untracked files, and a HEAD that is not the tag v<version>, where <version> is
#      the workspace version in Cargo.toml;
#   2. builds the portable Linux binary of the machine it runs on (x86_64 or aarch64) with scripts/build-portable.sh,
#      which links against glibc 2.34 and fails when the binary needs a newer one, so the asset runs on Linux of that
#      architecture with glibc 2.34 or newer (Enterprise Linux 9, Ubuntu 22.04, Debian 12, and newer). The release
#      workflow runs this script on an x86_64 machine and on an aarch64 one, and publishes both binaries;
#   3. checks that the binary answers `--version` with this version and this commit;
#   4. writes dist/arca-<version>-linux-<architecture>, dist/install.sh (a copy of scripts/install.sh, the installer),
#      dist/SHA256SUMS (the checksums of those two) and dist/RELEASE-NOTES.md (this version's CHANGELOG.md section);
#   5. prints the `gh release create` command for those files (not with --rehearse).
#
# It refuses, before it builds, an installer that does not install this version: the ARCA_VERSION line of
# scripts/install.sh must name it. scripts/version.sh set writes that line with every other.
#
# Run one cargo build at a time on a shared build machine: this script starts one.
set -euo pipefail

fail() {
    echo "release: $*" >&2
    exit 1
}

rehearse=0
case "${1-}" in
    "") ;;
    --rehearse) rehearse=1 ;;
    *) fail "usage: scripts/release.sh [--rehearse]" ;;
esac
[ "$#" -le 1 ] || fail "usage: scripts/release.sh [--rehearse]"

for tool in git cargo sha256sum sed awk install; do
    command -v "$tool" >/dev/null || fail "$tool is not on PATH"
done

repo="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

# The version is stated once, in the workspace manifest.
version="$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)"
[ -n "$version" ] || fail "no version under [workspace.package] in Cargo.toml"
tag="v$version"

# A release is built from exactly what the tag names: nothing uncommitted, nothing untracked.
[ -z "$(git status --porcelain)" ] ||
    fail "the tree is not clean; commit or remove what \`git status\` lists, then run again"
commit="$(git rev-parse --short HEAD)"
head_tags="$(git tag --points-at HEAD)"
if printf '%s\n' "$head_tags" | grep -qxF -- "$tag"; then
    rehearse_note=""
elif [ "$rehearse" = 1 ]; then
    rehearse_note="REHEARSAL: HEAD ($commit) is not the tag $tag. These files are not the release."
    echo "release: $rehearse_note" >&2
else
    fail "HEAD ($commit) is not the tag $tag; check out the tag, or pass --rehearse to try the script without it"
fi

# This version's section of the changelog becomes the release notes.
[ -f CHANGELOG.md ] || fail "CHANGELOG.md is missing"
notes="$(awk -v want="## $version" '
    index($0, "## ") == 1 { inside = ($0 == want || index($0, want " ") == 1); if (inside) next }
    inside { print }
' CHANGELOG.md)"
[ -n "$(printf '%s' "$notes" | tr -d '[:space:]')" ] ||
    fail "CHANGELOG.md has no section headed \"## $version\""

# The installer goes out with the release, so it must install this release: its one ARCA_VERSION line names it.
[ -f scripts/install.sh ] || fail "scripts/install.sh is missing"
installs="$(sed -n 's/^ARCA_VERSION=//p' scripts/install.sh)"
[ "$installs" = "$version" ] ||
    fail "scripts/install.sh installs \"$installs\", not $version: its ARCA_VERSION line must name this release (scripts/version.sh set writes it)"

# The build prints its own report; the lines this script needs are read back from it.
report="$(scripts/build-portable.sh | tee /dev/stderr)" || fail "scripts/build-portable.sh failed"
binary="$(printf '%s\n' "$report" | sed -n 's/^binary: *//p')"
glibc="$(printf '%s\n' "$report" | sed -n 's/^max GLIBC: *//p')"
[ -x "$binary" ] || fail "the build reported no binary"
[ -n "$glibc" ] || fail "the build reported no GLIBC version"

# The binary must say it is this version, built from this commit.
case "$(uname -m)" in
    x86_64 | aarch64) arch="$(uname -m)" ;;
    *) fail "this script builds on x86_64 or aarch64 Linux only (found $(uname -m))" ;;
esac
expected="Automagik Arca $version ($commit, $arch-unknown-linux-gnu)"
said="$("$binary" --version)" || fail "$binary --version failed"
[ "$said" = "$expected" ] || fail "the binary says \"$said\", not \"$expected\""

asset="arca-$version-linux-$arch"
dist="$repo/dist"
rm -rf "$dist"
mkdir "$dist"
install -m 0755 "$binary" "$dist/$asset"
# The installer as the tag holds it: the tree is clean, so this file is the commit's.
install -m 0755 scripts/install.sh "$dist/install.sh"
(cd "$dist" && sha256sum "$asset" install.sh >SHA256SUMS && sha256sum --check --strict --quiet SHA256SUMS) ||
    fail "could not write or check dist/SHA256SUMS"
{
    printf '%s\n' "$notes" | sed -e '/./,$!d'
    printf '\n`arca-%s-linux-x86_64` is one file for x86_64 Linux and `arca-%s-linux-aarch64` one for aarch64 (ARM) Linux.' \
        "$version" "$version"
    printf ' Each is built against glibc 2.34 and needs glibc %s or newer.' "$glibc"
    printf ' `install.sh` sets up a read-only node with this release: read it, then run `sh install.sh`.'
    printf ' Check what you downloaded with `sha256sum --check --ignore-missing SHA256SUMS`, and stop unless it prints `arca-%s-linux-x86_64: OK` or `arca-%s-linux-aarch64: OK`, for the file you took.\n' \
        "$version" "$version"
} >"$dist/RELEASE-NOTES.md"

echo
[ -z "$rehearse_note" ] || echo "$rehearse_note"
echo "version:  $said"
echo "glibc:    the binary needs glibc $glibc or newer"
echo "assets:   dist/$asset"
echo "          dist/install.sh"
echo "          dist/SHA256SUMS"
echo "          dist/RELEASE-NOTES.md"
echo "sha256:   $(sed -n 1p "$dist/SHA256SUMS" | cut -d ' ' -f 1)"
echo
if [ "$rehearse" = 1 ]; then
    echo "Nothing was published. This was a rehearsal, so no publish command is printed:"
    echo "run scripts/release.sh without --rehearse, at the tag $tag, to get it."
    exit 0
fi
echo "Nothing was published. To publish, push the tag $tag, then run:"
echo
echo "  gh release create $tag --repo automagik-si/arca --verify-tag --latest \\"
echo "    --title \"Automagik Arca $version\" --notes-file dist/RELEASE-NOTES.md \\"
echo "    dist/$asset dist/install.sh dist/SHA256SUMS"
