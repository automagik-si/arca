#!/bin/bash
# The rebuild-on-new-release flow. For each image line (releases/<line>.env), ask GitHub for the newest release of that
# line in actions/runner-images; when it is newer than the pin, move the pins to it and, with --build, build it.
#
#   images/github-runner/check-release.sh [--build] [<line>...]      (default: every line in releases/)
#
# It prints one line per image line: "<line> current <tag>" or "<line> new <old tag> -> <new tag>". It exits 0 when
# nothing changed, 10 when a pin moved (so a timer or an Arca function can act on it), and something else on error.
# The pins are a reviewed change: commit releases/<line>.env and docs/runner-image/<new tag>/ together (the build
# writes the latter), and promote the image only once the compatibility test (compat/) has passed on it.
#
# Reads GitHub's public API without a credential (60 requests an hour), or with GH_TOKEN when it is set.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
build=no
lines=()
for arg in "$@"; do
  case $arg in
    --build) build=yes ;;
    -*) echo "check-release.sh: unknown argument $arg" >&2; exit 2 ;;
    *) lines+=("$arg") ;;
  esac
done
if [ ${#lines[@]} -eq 0 ]; then
  for f in "$here"/releases/*.env; do
    [ "$(basename "$f")" = tools.env ] || lines+=("$(basename "$f" .env)")
  done
fi

api() {
  local auth=()
  [ -n "${GH_TOKEN:-}" ] && auth=(-H "Authorization: Bearer $GH_TOKEN")
  curl -fsSL --retry 3 -H 'Accept: application/vnd.github+json' "${auth[@]}" "https://api.github.com/repos/actions/runner-images/$1"
}

moved=0
for line in "${lines[@]}"; do
  env_file=$here/releases/$line.env
  current=$(sed -n 's/^RUNNER_IMAGES_TAG=//p' "$env_file")
  os=$(sed -n 's/^IMAGE_OS=//p' "$env_file")
  release=$(api 'releases?per_page=50' | jq -c --arg p "$os/" \
    '[.[] | select((.tag_name | startswith($p)) and (.prerelease | not) and (.draft | not))] | sort_by(.published_at) | last')
  newest=$(jq -r '.tag_name // empty' <<<"$release")
  if [ -z "$newest" ]; then
    echo "$line: no release of $os found in the last 50" >&2
    exit 1
  fi
  if [ "$newest" = "$current" ]; then
    echo "$line current $current"
    continue
  fi
  commit=$(api "git/ref/tags/$newest" | jq -r '.object.sha')
  version=$(jq -r '.body' <<<"$release" | sed -n 's/^- Image Version: *//p' | head -1 | tr -d '\r')
  if [ -z "$commit" ] || [ -z "$version" ]; then
    echo "$line: release $newest has no commit or no 'Image Version' line" >&2
    exit 1
  fi
  sed -i -e "s|^RUNNER_IMAGES_TAG=.*|RUNNER_IMAGES_TAG=$newest|" -e "s|^RUNNER_IMAGES_COMMIT=.*|RUNNER_IMAGES_COMMIT=$commit|" \
    -e "s|^IMAGE_VERSION=.*|IMAGE_VERSION=$version|" "$env_file"
  echo "$line new $current -> $newest"
  moved=1
  if [ "$build" = yes ]; then
    "$here/build.sh" "$line"
  fi
done
[ "$moved" -eq 0 ] || exit 10
