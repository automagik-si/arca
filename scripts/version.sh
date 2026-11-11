#!/usr/bin/env bash
# Automagik Arca's one version. The whole repository carries it: the workspace's crates (Cargo.toml and Cargo.lock)
# and the canvas package (package.json and package-lock.json). Nobody writes it by hand: the release workflow
# (.github/workflows/release.yml) works out the next one from the release tags and writes it with this script.
#
# A release is 0.N.P, tagged v0.N.P:
#   - a full release is 0.N.0, N one above the highest v0.N.0 tag, numbers compared as numbers. It is cut when
#     anything the binary is built from changed.
#   - a patch is 0.N.(P+1) on the newest N. It is cut when what changed ships, but the binary is not built from it.
#   - nothing is cut when everything that changed is inert: documents, tests, the workflows.
# The first release under this rule is 0.8.0. The seven before it were v0.1.0-alpha.1 to v0.1.0-alpha.7.
#
#   scripts/version.sh check                       every field carries the workspace version; exit 1 when not
#   scripts/version.sh set <version>               write it everywhere
#   scripts/version.sh next                        the next full release
#   scripts/version.sh next-patch                  the next patch release
#   scripts/version.sh next-for <full|patch>       the version a release of that kind takes: next for full; next-patch
#                                                  for patch, or next while no 0.N.P release exists to patch
#   scripts/version.sh last                        the newest release tag in HEAD's history, or nothing
#   scripts/version.sh kind [<tag>]                full | patch | none: what the changes since <tag> (default: last) need
#   scripts/version.sh changelog <version> <date> [<tag>]
#                                                  write the "## <version> (<date>)" section of CHANGELOG.md: the text
#                                                  under "## Unreleased", then one line per change since <tag>. It
#                                                  only adds lines; the release workflow refuses anything else
#
# ARCA_VERSION_ROOT names another tree to work on; the tests use it.
set -euo pipefail

# The settings. Each is stated here and nowhere else.
MAJOR=0
# The first minor this rule may cut: 0.8.0 follows v0.1.0-alpha.7.
FIRST_MINOR=8
# The last release of the old numbering. Until a 0.N.P tag exists, changes are counted from it.
LEGACY_LAST="v0.1.0-alpha.7"
# A change under one of these ships nothing: no release is cut for it. Extended regular expressions on the path.
INERT=(
    '^docs/'
    '\.md$'
    '^\.github/'
    '^LICENSE$'
    '^NOTICE$'
    '^\.gitignore$'
    '^crates/[^/]+/tests/'
    '^packages/[^/]+/test/'
    # A script only the workflows call. scripts/release.sh and scripts/version.sh are not among them: a person runs
    # them too, and they are in the patch list below.
    '^scripts/ci-[^/]+$'
)
# A change under one of these ships, but the binary is not built from it: a patch. scripts/build-portable.sh is not
# here, because its flags change the binary.
PATCH=(
    '^packages/'
    '^deploy/'
    '^schema/'
    '^scripts/(release|version)\.sh$'
)
# Every other path is one the binary is built from, or one nobody has classed yet: a full release.

fail() {
    echo "version: $*" >&2
    exit 1
}

root="${ARCA_VERSION_ROOT:-$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}"
[ -d "$root" ] || fail "$root is not a directory"

RELEASE='^v'"$MAJOR"'\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
VERSION='^'"$MAJOR"'\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'

# Every release tag as "<minor> <patch>", one per line. Tags of another major, or with anything after the patch, are
# not releases of this rule.
releases() {
    git -C "$root" tag --list "v$MAJOR.*" | grep -E "$RELEASE" | sed -E 's/^v[0-9]+\.([0-9]+)\.([0-9]+)$/\1 \2/' || true
}

# The lines that carry the version, as "<file>:<line number>", one per field. A field that cannot be found is printed
# as "<file>:0 <what is missing>".
fields() {
    local file line name
    file="Cargo.toml"
    line="$(awk '/^\[/ { inside = ($0 == "[workspace.package]") } inside && /^version = "[^"]*"$/ { print NR; exit }' "$root/$file")"
    echo "$file:${line:-0} [workspace.package] version"
    file="Cargo.lock"
    for manifest in "$root"/crates/*/Cargo.toml; do
        name="$(awk '/^\[/ { inside = ($0 == "[package]") } inside && /^name = "[^"]*"$/ { gsub(/^name = "|"$/, ""); print; exit }' "$manifest")"
        [ -n "$name" ] || { echo "${manifest#"$root"/}:0 [package] name"; continue; }
        # The crate's own entry: the version line of the [[package]] whose name is the crate's. An entry that ends
        # with no version line finds nothing, so the next entry's version is never taken for it.
        line="$(awk -v want="name = \"$name\"" '
            /^\[/ { if (named) exit; table = $0; next }
            table == "[[package]]" && $0 == want { named = 1 }
            named && /^version = "[^"]*"$/ { print NR; exit }
        ' "$root/$file")"
        echo "$file:${line:-0} [[package]] $name version"
    done
    for file in packages/arca-canvas-dsh/package.json packages/arca-canvas-dsh/package-lock.json; do
        line="$(awk '/^  "version": "[^"]*",?$/ { print NR; exit }' "$root/$file")"
        echo "$file:${line:-0} version"
    done
    file="packages/arca-canvas-dsh/package-lock.json"
    line="$(awk '
        /^    "": \{$/ { inside = 1; next }
        inside && /^    \}/ { exit }
        inside && /^      "version": "[^"]*",?$/ { print NR; exit }
    ' "$root/$file")"
    echo "$file:${line:-0} packages[\"\"].version"
}

# The version one field line carries.
value_at() {
    sed -n "${2}p" "$root/$1" | sed -E 's/^[^"]*("version": |version = )"([^"]*)".*$/\2/'
}

workspace_version() {
    local at
    at="$(fields)"
    at="${at%%$'\n'*}"
    at="${at%% *}"
    [ "${at##*:}" != 0 ] || fail "no version under [workspace.package] in Cargo.toml"
    value_at "${at%%:*}" "${at##*:}"
}

check() {
    local want bad=0 at what value
    want="$(workspace_version)"
    while read -r at what; do
        if [ "${at##*:}" = 0 ]; then
            echo "version: ${at%%:*} has no $what" >&2
            bad=1
            continue
        fi
        value="$(value_at "${at%%:*}" "${at##*:}")"
        if [ "$value" != "$want" ]; then
            echo "version: ${at%%:*} $what = $value, not $want" >&2
            bad=1
        fi
    done < <(fields)
    [ "$bad" = 0 ] || exit 1
    echo "version: $want everywhere"
}

set_version() {
    local version="$1" at what all
    [[ "$version" =~ $VERSION ]] || fail "not a version: $version (want $MAJOR.N.P)"
    # Every field is found before any file is written, so a missing one changes nothing.
    all="$(fields)"
    while read -r at what; do
        [ "${at##*:}" != 0 ] || fail "${at%%:*} has no $what; nothing was written"
    done <<<"$all"
    while read -r at what; do
        sed -i -E "${at##*:}s/(\"version\": |version = )\"[^\"]*\"/\\1\"$version\"/" "$root/${at%%:*}"
    done <<<"$all"
    echo "version: $version"
}

next() {
    local highest
    highest="$(releases | awk '$2 == 0 { print $1 }' | sort -n | tail -n 1)"
    if [ -z "$highest" ] || [ "$((highest + 1))" -lt "$FIRST_MINOR" ]; then
        echo "$MAJOR.$FIRST_MINOR.0"
    else
        echo "$MAJOR.$((highest + 1)).0"
    fi
}

next_patch() {
    local minor patch
    minor="$(releases | awk '{ print $1 }' | sort -n | tail -n 1)"
    [ -n "$minor" ] || fail "no release to patch"
    patch="$(releases | awk -v minor="$minor" '$1 == minor { print $2 }' | sort -n | tail -n 1)"
    echo "$MAJOR.$minor.$((patch + 1))"
}

next_for() {
    case "$1" in
        full) next ;;
        patch) if [ -n "$(releases)" ]; then next_patch; else next; fi ;;
        *) fail "not a kind of release: $1 (want full or patch)" ;;
    esac
}

# The newest release tag that HEAD's history holds. A tag on a commit HEAD never got still takes its number (next and
# next-patch read every tag), but nothing is counted from it.
last() {
    local minor patch tag
    while read -r minor patch; do
        tag="v$MAJOR.$minor.$patch"
        if git -C "$root" merge-base --is-ancestor "$tag^{commit}" HEAD 2>/dev/null; then
            echo "$tag"
            return 0
        fi
    done < <(releases | sort -k1,1nr -k2,2nr)
    if git -C "$root" rev-parse -q --verify "refs/tags/$LEGACY_LAST" >/dev/null &&
        git -C "$root" merge-base --is-ancestor "$LEGACY_LAST^{commit}" HEAD 2>/dev/null; then
        echo "$LEGACY_LAST"
    fi
}

matches_any() {
    local path="$1" pattern
    shift
    for pattern in "$@"; do
        [[ "$path" =~ $pattern ]] && return 0
    done
    return 1
}

kind() {
    local since="${1-}" path result=none
    [ "$#" -ge 1 ] || since="$(last)"
    if [ -z "$since" ]; then
        # The first release: everything since the beginning ships.
        echo full
        return 0
    fi
    git -C "$root" rev-parse -q --verify "$since^{commit}" >/dev/null || fail "no such tag or commit: $since"
    # Renames are listed as a removal and an addition, so a file that left a shipping path counts.
    while IFS= read -r path; do
        [ -n "$path" ] || continue
        matches_any "$path" "${INERT[@]}" && continue
        if matches_any "$path" "${PATCH[@]}"; then
            result="patch"
        else
            echo full
            return 0
        fi
    done < <(git -C "$root" diff --name-only --no-renames "$since" HEAD)
    echo "$result"
}

# One line per change since a tag: the subjects on dev's own line (a merge counts once, by its subject), without the
# release commits and the commits that ship nothing by their own word (ci, docs, test), and without the "merge: " word
# and the review note a merge subject ends with.
#
# A subject is text someone else wrote, and it goes into CHANGELOG.md and the release notes. It is never run or
# expanded. Its control characters are removed (a bare carriage return would be shown as a line break, and what
# follows it as a line of its own), and a leading "#" is escaped, so no subject can stand as a heading.
subjects() {
    local range="HEAD"
    [ -z "${1-}" ] || range="$1..HEAD"
    git -C "$root" log --first-parent --format=%s "$range" |
        LC_ALL=C tr -d '\000-\011\013-\037\177' |
        grep -vE '^(chore\(release\)|ci|docs|test)(\([^)]*\))?!?: ' |
        sed -E 's/^merge: //; s/ \(([^()]*)?review[^()]*\)$//; s/^[[:space:]]+//; s/^#/\\#/' || true
}

# Writes the section of a release into CHANGELOG.md. Lines are only added: none of the file's own lines is removed,
# changed or moved. The heading goes in above the first line someone wrote under "## Unreleased" (so that text leads
# the section), or at the end of the Unreleased section when nothing was written; the lines for the changes go in at
# the end of the section. The comment that explains the Unreleased heading stays where it is.
changelog() {
    local version="$1" date="$2" since file="$root/CHANGELOG.md" at next total first lines before tmp
    [[ "$version" =~ $VERSION ]] || fail "not a version: $version (want $MAJOR.N.P)"
    [[ "$date" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || fail "not a date: $date (want YYYY-MM-DD)"
    if [ "$#" -ge 3 ]; then since="$3"; else since="$(last)"; fi
    [ -f "$file" ] || fail "CHANGELOG.md is missing"
    at="$(grep -nxF '## Unreleased' "$file" | cut -d : -f 1 | sed -n 1p)"
    [ -n "$at" ] || fail "CHANGELOG.md has no \"## Unreleased\" heading"
    ! grep -qE "^## ${version//./\\.}( |\$)" "$file" || fail "CHANGELOG.md already has a section for $version"
    # The Unreleased section runs to the next "## " heading, or to the end of the file.
    total="$(wc -l <"$file")"
    next="$(awk -v at="$at" 'NR > at && /^## / { print NR; exit }' "$file")"
    [ -n "$next" ] || next="$((total + 1))"
    # The first line someone wrote in it: not blank, and not in a comment (a comment starts a line with "<!--" and
    # ends a line with "-->").
    first="$(awk -v at="$at" -v end="$next" '
        NR <= at || NR >= end { next }
        !inside && /^<!--/ { inside = 1 }
        inside { if (/-->$/) inside = 0; next }
        /[^[:space:]]/ { print NR; exit }
    ' "$file")"
    lines="$(subjects "$since" | sed -e 's/^/- /')"
    [ -n "$first$lines" ] || lines="- No change is listed for this release."
    before="$(sed -n "$((next - 1))p" "$file")"
    tmp="$(mktemp "$file.XXXXXX")"
    {
        if [ -n "$first" ]; then
            sed -n "1,$((first - 1))p" "$file"
            printf '## %s (%s)\n\n' "$version" "$date"
            sed -n "${first},$((next - 1))p" "$file"
        else
            sed -n "1,$((next - 1))p" "$file"
            [ -z "$before" ] || echo
            printf '## %s (%s)\n\n' "$version" "$date"
            before=""
        fi
        if [ -n "$lines" ]; then
            [ -z "$before" ] || echo
            printf '%s\n' "$lines"
            [ "$next" -gt "$total" ] || echo
        fi
        [ "$next" -gt "$total" ] || sed -n "${next},\$p" "$file"
    } >"$tmp"
    mv "$tmp" "$file"
    echo "changelog: $version ($date)"
}

command="${1-}"
[ "$#" -eq 0 ] || shift
case "$command" in
    check) [ "$#" -eq 0 ] || fail "check takes no argument"; check ;;
    set) [ "$#" -eq 1 ] || fail "usage: scripts/version.sh set <version>"; set_version "$1" ;;
    next) [ "$#" -eq 0 ] || fail "next takes no argument"; next ;;
    next-patch) [ "$#" -eq 0 ] || fail "next-patch takes no argument"; next_patch ;;
    next-for) [ "$#" -eq 1 ] || fail "usage: scripts/version.sh next-for <full|patch>"; next_for "$1" ;;
    last) [ "$#" -eq 0 ] || fail "last takes no argument"; last ;;
    kind) [ "$#" -le 1 ] || fail "usage: scripts/version.sh kind [<tag>]"; kind "$@" ;;
    changelog) [ "$#" -eq 2 ] || [ "$#" -eq 3 ] || fail "usage: scripts/version.sh changelog <version> <date> [<tag>]"; changelog "$@" ;;
    *)
        echo "usage: scripts/version.sh check | set <version> | next | next-patch | next-for <full|patch> | last | kind [<tag>] | changelog <version> <date> [<tag>]" >&2
        exit 2
        ;;
esac
