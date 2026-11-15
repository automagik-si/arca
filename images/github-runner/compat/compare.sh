#!/bin/bash
# Put side by side what each job of one compat.yml run saw on GitHub's runner and on Arca's.
#
#   compare.sh <run id> <owner/repo> [<output.md>]
#
# Needs gh, signed in with read access to the test repository. Writes Markdown (to the file, or standard output):
# first every job with its result on both sides, then every check whose value differs, then the count of checks that
# match. Exits 0 when everything matches, 1 when something differs.
set -euo pipefail
run=${1:?usage: compare.sh <run id> <owner/repo> [<output.md>]}
repo=${2:?usage: compare.sh <run id> <owner/repo> [<output.md>]}
output=${3:-/dev/stdout}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

gh run download "$run" -R "$repo" -D "$work" >/dev/null
gh run view "$run" -R "$repo" --json jobs -q '.jobs[] | [.name, (.conclusion // .status)] | @tsv' >"$work/jobs.tsv"

differ=0
{
  echo "# Compatibility run $run ($repo)"
  echo
  echo "## Jobs"
  echo
  echo "| job | GitHub | Arca |"
  echo "|---|---|---|"
  # Job names look like "docker (github)" or "fingerprint (arca, tools)": the side is the first matrix value.
  awk -F'\t' '{
      name = $1; side = "both"
      if (match(name, /\((github|arca)(, |\))/)) { side = substr(name, RSTART + 1, RLENGTH - 2); sub(/,? ?$/, "", side)
        base = name; sub(/\((github|arca)(, )?/, "(", base); sub(/ \(\)$/, "", base) } else base = name
      result[base, side] = $2; if (!(base in seen)) { seen[base] = 1; order[++n] = base }
    }
    END { for (i = 1; i <= n; i++) { b = order[i]
      g = result[b, "github"]; a = result[b, "arca"]; if (g == "" && a == "") { g = result[b, "both"]; a = "-" }
      printf "| %s | %s | %s |\n", b, (g == "" ? "-" : g), (a == "" ? "-" : a) } }' "$work/jobs.tsv"
  echo
  echo "## Checks that differ"
  echo
  echo "| check | GitHub | Arca |"
  echo "|---|---|---|"
  same=0
  for github_dir in "$work"/fp-*-github; do
    [ -d "$github_dir" ] || continue
    job=$(basename "$github_dir" -github)
    arca_dir=$work/$job-arca
    [ -f "$arca_dir/fp.tsv" ] || : >"$work/empty.tsv"
    # Fields are split on \x1f, not on tabs: bash joins two tabs in a row, which would move a lone value across.
    while IFS=$'\x1f' read -r check github arca; do
      if [ "$github" = "$arca" ]; then
        same=$((same + 1))
      else
        printf '| %s: %s | %s | %s |\n' "${job#fp-}" "$check" "${github:-<none>}" "${arca:-<none>}" | sed 's/|  |/| |/g'
        differ=1
      fi
    done < <(awk -F'\t' 'FNR == NR { g[$1] = $2; keys[++n] = $1; next } { a[$1] = $2; if (!($1 in g)) keys[++n] = $1 }
        END { for (i = 1; i <= n; i++) { k = keys[i]; if (!(k in done)) { done[k] = 1; printf "%s\037%s\037%s\n", k, g[k], a[k] } } }' \
      "$github_dir/fp.tsv" "$( [ -f "$arca_dir/fp.tsv" ] && echo "$arca_dir/fp.tsv" || echo "$work/empty.tsv")")
  done
  echo
  echo "$same checks match."
  echo "$differ" >"$work/differ"
} >"$output"
[ "$(cat "$work/differ")" -eq 0 ]
