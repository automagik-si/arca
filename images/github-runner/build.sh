#!/bin/bash
# Build Arca's GitHub Actions runner image from GitHub's own scripts (actions/runner-images), pinned to one release.
#
#   images/github-runner/build.sh <line> [--keep] [--tests strict|record] [--no-publish] [--no-gate]
#
# <line> is a file in releases/ without .env: ubuntu24 or ubuntu26. What it does, in order:
#
# 1. Waits for the host's gate: CPU idle >= GATE_IDLE % over 5 s, MemAvailable >= GATE_MEM_GIB, and room on the
#    storage pool (GATE_POOL_GIB) and under /var/lib/incus (GATE_ROOT_GIB). It checks again every minute, up to
#    GATE_WAIT seconds. One build per host at a time (a lock in WORK).
# 2. Fetches GitHub's repository at the pinned tag (and checks the commit), Packer (checked by sha256) and GitHub's
#    own software report for that release (internal.<line>.json).
# 3. Starts a virtual machine `arca-runner-build-<line>` in the Incus project PROJECT from BASE_IMAGE, capped at CPU
#    CPUs, MEMORY and a DISK root throttled to DISK_READ/DISK_WRITE, with a build user `packer` reached by a key made
#    for this build only. vm/prepare.sh puts in place what GitHub's scripts expect from an Azure machine.
# 4. Runs GitHub's Packer template, changed only as patch-template.py says, against that machine: every GitHub script
#    in GitHub's order, their image tests, their software report.
# 5. Compares our software report with GitHub's for the same release, with GitHub's own comparer
#    (Calculate-ImagesDifference.ps1), and with our previous build of the line when there is one.
# 6. vm/finalize.sh: the runner user, the runner program, arca-runner.service, then the clean-up of the build.
# 7. Stops the machine and publishes it as the Incus image `arca/github-runner/<tag>` (in PROJECT's image store), then
#    deletes the machine. --no-publish, or too little room under /var/lib/incus, keeps the stopped machine instead,
#    with a snapshot `image`: `incus copy` makes job machines from it (copy-on-write on btrfs and ZFS).
# 8. Writes the published difference into docs/runner-image/<tag>/ (README.md, software-report.json, the diffs, the
#    failed tests) and deletes everything else it made, except a compressed log in WORK/logs.
#
# --keep keeps the machine (after a failure too); --tests strict stops at the first failed GitHub test, as on GitHub
# (default record: patch-template.py); --no-gate skips step 1's waiting (the room checks still apply).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
line=${1:?usage: build.sh <line> [--keep] [--tests strict|record] [--no-publish] [--no-gate]}
shift
keep=no tests=record publish=yes gate=yes
while [ $# -gt 0 ]; do
  case $1 in
    --keep) keep=yes ;;
    --tests) tests=${2:?}; shift ;;
    --no-publish) publish=no ;;
    --no-gate) gate=no ;;
    *) echo "build.sh: unknown argument $1" >&2; exit 2 ;;
  esac
  shift
done
[ -f "$here/releases/$line.env" ] || { echo "build.sh: no releases/$line.env" >&2; exit 2; }
# shellcheck source=/dev/null
. "$here/releases/$line.env"
# shellcheck source=/dev/null
. "$here/releases/tools.env"

: "${PROJECT:=ci}"
: "${CPU:=4}"
: "${MEMORY:=8GiB}"
: "${DISK:=80GiB}"
: "${DISK_READ:=150MB}"
: "${DISK_WRITE:=80MB}"
: "${WORK:=$HOME/.cache/arca-runner-image}"
: "${GATE_IDLE:=30}"
: "${GATE_MEM_GIB:=12}"
: "${GATE_POOL_GIB:=90}"     # GitHub's image uses about 55-60 GB; the rest is margin for the pool's other users
: "${GATE_ROOT_GIB:=20}"
: "${GATE_WAIT:=43200}"
: "${PUBLISH_COMPRESSION:=zstd}"
: "${DIFF_URL_BASE:=https://github.com/automagik-si/arca/blob/dev/docs/runner-image}"

slug=${RUNNER_IMAGES_TAG//\//-}
vm=arca-runner-build-$IMAGE_OS
alias=arca/github-runner/$RUNNER_IMAGES_TAG
src=$WORK/$slug
out=$repo/docs/runner-image/$slug
log=$WORK/logs/$slug-$(date -u +%Y%m%dT%H%M%SZ).log
mkdir -p "$WORK/logs" "$WORK/tools"
find "$WORK/logs" -name '*.log' -exec gzip -f {} + 2>/dev/null || true
exec > >(while IFS= read -r l; do printf '%s %s\n' "$(date -u +%FT%TZ)" "$l"; done | tee -a "$log") 2>&1

say() { printf 'build: %s\n' "$*"; }
gib() { echo $(( $1 / 1073741824 )); }
in_vm() { incus exec "$vm" --project "$PROJECT" "$@"; }

exec 9>"$WORK/build.lock"
flock -n 9 || { say "another image build is running on this host (lock $WORK/build.lock)"; exit 1; }

pool=$(incus profile device get default root pool --project "$PROJECT")
pool_free() { incus query "/1.0/storage-pools/$pool/resources" | jq '.space.total - .space.used'; }
root_free() { df -B1 --output=avail /var/lib/incus | tail -1; }
cpu_idle() {
  local a b
  read -r _ a1 a2 a3 a4 a5 a6 a7 a8 _ </proc/stat
  a=$((a1 + a2 + a3 + a4 + a5 + a6 + a7 + a8)) ai=$((a4 + a5))
  sleep 5
  read -r _ b1 b2 b3 b4 b5 b6 b7 b8 _ </proc/stat
  b=$((b1 + b2 + b3 + b4 + b5 + b6 + b7 + b8)) bi=$((b4 + b5))
  echo $(( 100 * (bi - ai) / (b - a) ))
}
mem_available() { awk '/^MemAvailable:/ { print $2 * 1024 }' /proc/meminfo; }

# --- 1. the gate --------------------------------------------------------------------------------------------------
t_start=$(date +%s)
if incus info "$vm" --project "$PROJECT" >/dev/null 2>&1; then
  say "the machine $vm already exists in project $PROJECT (a build was kept or is running): delete it first"
  exit 1
fi
waited=0
while :; do
  idle=$(cpu_idle) mem=$(mem_available) pf=$(pool_free) rf=$(root_free)
  why=''
  [ "$(gib "$pf")" -ge "$GATE_POOL_GIB" ] || why="$why pool $pool has $(gib "$pf") GiB free (needs $GATE_POOL_GIB);"
  [ "$(gib "$rf")" -ge "$GATE_ROOT_GIB" ] || why="$why /var/lib/incus has $(gib "$rf") GiB free (needs $GATE_ROOT_GIB);"
  if [ "$gate" = yes ]; then
    [ "$idle" -ge "$GATE_IDLE" ] || why="$why CPU idle $idle% (needs $GATE_IDLE);"
    [ "$(gib "$mem")" -ge "$GATE_MEM_GIB" ] || why="$why MemAvailable $(gib "$mem") GiB (needs $GATE_MEM_GIB);"
  fi
  [ -n "$why" ] || break
  if [ "$waited" -ge "$GATE_WAIT" ]; then
    say "the gate stayed closed for $waited s:$why giving up"
    exit 3
  fi
  [ $((waited % 600)) -eq 0 ] && say "waiting for the gate:$why"
  sleep 55
  waited=$(( $(date +%s) - t_start ))
done
say "gate open: CPU idle $idle%, MemAvailable $(gib "$mem") GiB, pool $(gib "$pf") GiB free, /var/lib/incus $(gib "$rf") GiB free"
t_gate=$(date +%s)

# --- 2. sources ---------------------------------------------------------------------------------------------------
cleanup() {
  local status=$?
  if [ "$status" -ne 0 ] && [ "$keep" = no ] && incus info "$vm" --project "$PROJECT" >/dev/null 2>&1; then
    say "failed (status $status): deleting $vm (use --keep to keep it)"
    incus delete -f "$vm" --project "$PROJECT" || true
  fi
  rm -rf "$src"
}
trap cleanup EXIT

packer=$WORK/tools/packer-$PACKER_VERSION
if [ ! -x "$packer" ]; then
  say "fetching Packer $PACKER_VERSION"
  curl -fsSL --retry 5 -o "$WORK/tools/packer.zip" \
    "https://releases.hashicorp.com/packer/$PACKER_VERSION/packer_${PACKER_VERSION}_linux_amd64.zip"
  printf '%s  %s\n' "$PACKER_SHA256" "$WORK/tools/packer.zip" | sha256sum -c --quiet
  unzip -p "$WORK/tools/packer.zip" packer >"$packer.new" && chmod 0755 "$packer.new" && mv "$packer.new" "$packer"
  rm -f "$WORK/tools/packer.zip"
fi

rm -rf "$src"
mkdir -p "$src"
say "fetching actions/runner-images $RUNNER_IMAGES_TAG"
git clone -q --depth 1 --branch "$RUNNER_IMAGES_TAG" https://github.com/actions/runner-images.git "$src/ri" 2>/dev/null
commit=$(git -C "$src/ri" rev-parse HEAD)
[ "$commit" = "$RUNNER_IMAGES_COMMIT" ] ||
  { say "the tag $RUNNER_IMAGES_TAG is commit $commit, not the pinned $RUNNER_IMAGES_COMMIT: GitHub moved it; stopping"; exit 1; }
curl -fsSL --retry 5 -o "$src/github-report.json" \
  "https://github.com/actions/runner-images/releases/download/$RUNNER_IMAGES_TAG/$GITHUB_REPORT"

ubuntu=$src/ri/images/ubuntu
tpl=$ubuntu/arca-templates
mkdir -p "$tpl"
cp "$ubuntu/templates/variable.ubuntu.pkr.hcl" "$ubuntu/templates/locals.ubuntu.pkr.hcl" "$here/source.arca.pkr.hcl" "$tpl/"
python3 -I "$here/patch-template.py" "$ubuntu/templates/$TEMPLATE" "$tpl/build.pkr.hcl" "$tests"
ssh-keygen -q -t ed25519 -N '' -C "arca-runner-image-build" -f "$src/key"

# --- 3. the build machine -----------------------------------------------------------------------------------------
say "starting $vm ($CPU CPUs, $MEMORY, $DISK root read $DISK_READ/s write $DISK_WRITE/s) from $BASE_IMAGE"
cat >"$src/user-data" <<EOF
#cloud-config
users:
  - name: packer
    uid: 1000
    shell: /bin/bash
    lock_passwd: true
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    ssh_authorized_keys:
      - $(cat "$src/key.pub")
ssh_pwauth: false
package_update: true
packages: [openssh-server]
EOF
incus init "$BASE_IMAGE" "$vm" --vm --project "$PROJECT" \
  -c limits.cpu="$CPU" -c limits.memory="$MEMORY" -c cloud-init.user-data="$(cat "$src/user-data")" \
  -d root,size="$DISK" -d root,limits.read="$DISK_READ" -d root,limits.write="$DISK_WRITE"
incus start "$vm" --project "$PROJECT"
for _ in $(seq 120); do in_vm -- true 2>/dev/null && break; sleep 5; done
# cloud-init says 2 for "done, with warnings". The Incus agent can go away while the machine boots (cloud-init
# restarts services): a lost connection is waited out, cloud-init's own answer is not.
cloud_init=255
for _ in $(seq 60); do
  answer=$(in_vm -- cloud-init status --wait 2>&1) && cloud_init=0 || cloud_init=$?
  case $answer in *"connect to the agent"*|*"agent isn't currently running"*) cloud_init=255; sleep 5; continue ;; esac
  break
done
if [ "$cloud_init" -ne 0 ] && [ "$cloud_init" -ne 2 ]; then
  say "cloud-init failed in $vm"
  in_vm -- cloud-init status --long || true
  exit 1
fi
ip=$(incus query "/1.0/instances/$vm/state?project=$PROJECT" | jq -r '[.network | to_entries[] | select(.key | test("^(en|eth)"))
  | .value.addresses[] | select(.family == "inet" and .scope == "global") | .address][0] // empty')
[ -n "$ip" ] || { say "$vm has no address"; exit 1; }
push_files() {
  local f
  in_vm -- mkdir -p /tmp/arca-image
  for f in "$@"; do incus file push "$here/vm/$f" "$vm/tmp/arca-image/$f" --project "$PROJECT"; done
}
push_files invoke-tests-record.sh prepare.sh
in_vm -- bash /tmp/arca-image/prepare.sh
t_vm=$(date +%s)
# --- 4. GitHub's template -----------------------------------------------------------------------------------------
say "running GitHub's template $TEMPLATE ($tests mode) against $vm at $ip"
(cd "$ubuntu" && PACKER_NO_COLOR=1 CHECKPOINT_DISABLE=1 "$packer" build -color=false -on-error=abort \
  -var "image_os=$IMAGE_OS" -var "image_version=$IMAGE_VERSION" \
  -var "arca_ssh_host=$ip" -var "arca_ssh_key=$src/key" arca-templates/)
t_packer=$(date +%s)

# --- 5. the difference ----------------------------------------------------------------------------------------------
say "comparing our software report with GitHub's"
mkdir -p "$out"
cp "$ubuntu/software-report.json" "$out/software-report.json"
cp "$ubuntu/$README" "$out/software-report.md"
in_vm -- mkdir -p /tmp/arca-diff
incus file push "$src/github-report.json" "$vm/tmp/arca-diff/github.json" --project "$PROJECT"
incus file push "$out/software-report.json" "$vm/tmp/arca-diff/ours.json" --project "$PROJECT"
comparer=/imagegeneration/SoftwareReport/software-report-base/Calculate-ImagesDifference.ps1
in_vm -- pwsh -NoProfile -File "$comparer" -PreviousJsonReportPath /tmp/arca-diff/github.json \
  -CurrentJsonReportPath /tmp/arca-diff/ours.json -OutputFile /tmp/arca-diff/github.md
incus file pull "$vm/tmp/arca-diff/github.md" "$out/against-github.md" --project "$PROJECT"
previous=$(find "$repo/docs/runner-image" -mindepth 1 -maxdepth 1 -type d -name "$IMAGE_OS-*" ! -name "$slug" | sort | tail -1)
if [ -n "$previous" ] && [ -f "$previous/software-report.json" ]; then
  incus file push "$previous/software-report.json" "$vm/tmp/arca-diff/previous.json" --project "$PROJECT"
  in_vm -- pwsh -NoProfile -File "$comparer" -PreviousJsonReportPath /tmp/arca-diff/previous.json \
    -CurrentJsonReportPath /tmp/arca-diff/ours.json -OutputFile /tmp/arca-diff/previous.md
  incus file pull "$vm/tmp/arca-diff/previous.md" "$out/against-previous.md" --project "$PROJECT"
fi
incus file pull "$vm/usr/local/lib/arca-image/test-failures.log" "$out/test-failures.txt" --project "$PROJECT"
in_vm -- rm -rf /tmp/arca-diff

# --- 6. runner, clean-up --------------------------------------------------------------------------------------------
say "finalizing"
# GitHub's template reboots the machine, which empties /tmp: the finishing files go in now.
push_files finalize.sh arca-runner-start arca-runner.service arca-swap.service
in_vm --env RUNNER_VERSION="$RUNNER_VERSION" --env RUNNER_SHA256="$RUNNER_SHA256" --env IMAGE_OS="$IMAGE_OS" \
  --env IMAGE_VERSION="$IMAGE_VERSION" --env RUNNER_IMAGES_TAG="$RUNNER_IMAGES_TAG" \
  --env ARCA_DIFF_URL="$DIFF_URL_BASE/$slug/README.md" -- bash /tmp/arca-image/finalize.sh
used=$(in_vm -- df -B1 --output=used / | tail -1 | tr -d ' ')
incus stop "$vm" --project "$PROJECT"
t_final=$(date +%s)

# --- 7. the image -------------------------------------------------------------------------------------------------
image_bytes='' kept=''
if [ "$publish" = yes ] && [ "$(root_free)" -ge $(( used * 2 + 10 * 1073741824 )) ]; then
  say "publishing $alias"
  incus image delete "$alias" --project "$PROJECT" 2>/dev/null || true
  incus publish "$vm" --project "$PROJECT" --alias "$alias" --compression "$PUBLISH_COMPRESSION"
  incus image set-property "$alias" description "GitHub runner image $RUNNER_IMAGES_TAG (Arca build)" --project "$PROJECT"
  image_bytes=$(incus image list --project "$PROJECT" -f json |
    jq --arg a "$alias" '[.[] | select(any(.aliases[]?; .name == $a)) | .size][0]')
  [ "$keep" = yes ] || incus delete "$vm" --project "$PROJECT"
else
  [ "$publish" = yes ] && say "not publishing: /var/lib/incus has $(gib "$(root_free)") GiB free, needs $(gib $(( used * 2 + 10 * 1073741824 ))) GiB"
  incus snapshot create "$vm" image --project "$PROJECT"
  kept="the stopped machine $vm, snapshot image, in project $PROJECT"
fi
t_end=$(date +%s)

# --- 8. the published difference ------------------------------------------------------------------------------------
hm() { printf '%dh%02dm' $(( $1 / 3600 )) $(( $1 % 3600 / 60 )); }
failed=$(grep -c . "$out/test-failures.txt" || true)
{
  echo "# Arca's GitHub runner image, $RUNNER_IMAGES_TAG"
  echo
  echo "Built from GitHub's own scripts in [actions/runner-images $RUNNER_IMAGES_TAG](https://github.com/actions/runner-images/releases/tag/${RUNNER_IMAGES_TAG/\//%2F}) (commit \`$RUNNER_IMAGES_COMMIT\`) by \`images/github-runner/build.sh $line\`."
  echo
  echo "| | |"
  echo "|---|---|"
  echo "| ImageOS, ImageVersion | \`$IMAGE_OS\`, \`$IMAGE_VERSION\` (the same as GitHub's image of this release) |"
  echo "| Runner program | $RUNNER_VERSION, in /home/runner/runners/$RUNNER_VERSION |"
  echo "| Built on | $(date -u +%F), a virtual machine of $CPU CPUs and $MEMORY, root disk throttled to $DISK_READ/s read and $DISK_WRITE/s write; Packer $PACKER_VERSION; base $BASE_IMAGE |"
  echo "| Build time | $(hm $((t_end - t_gate))) in all: machine $(hm $((t_vm - t_gate))), GitHub's template $(hm $((t_packer - t_vm))), report and finish $(hm $((t_final - t_packer))), image $(hm $((t_end - t_final))) |"
  echo "| Size | $(gib "$used") GiB used on the root disk$( [ -n "$image_bytes" ] && echo "; the Incus image file is $(gib "$image_bytes") GiB ($PUBLISH_COMPRESSION)") |"
  echo "| Result | $( [ -n "$image_bytes" ] && echo "Incus image \`$alias\`" || echo "$kept") |"
  if [ "$failed" -eq 0 ]; then
    echo "| GitHub's image tests | all passed |"
  else
    echo "| GitHub's image tests | $failed failed, listed below ($tests mode) |"
  fi
  echo
  cat "$here/CHANGES.md"
  if [ "$failed" -ne 0 ]; then
    echo
    echo "## GitHub's image tests that failed"
    echo
    sed 's/^/- /' "$out/test-failures.txt"
  fi
  echo
  echo "## Software: ours against GitHub's image of the same release"
  echo
  echo "Made with GitHub's comparer (\`helpers/software-report-base/Calculate-ImagesDifference.ps1\`): GitHub's report (\`$GITHUB_REPORT\` of the release) is the previous one, ours the current one. Tools GitHub's scripts install as \"latest\" can move between their build day and ours."
  echo
  cat "$out/against-github.md"
  if [ -f "$out/against-previous.md" ]; then
    echo
    echo "## Software: ours against our previous image ($(basename "$previous"))"
    echo
    cat "$out/against-previous.md"
  fi
  echo
  echo "Our full software list is [software-report.md](software-report.md) (GitHub's generator, run in our image)."
} >"$out/README.md"
rm -f "$out/against-github.md" "$out/against-previous.md"
[ "$failed" -ne 0 ] || rm -f "$out/test-failures.txt"
say "done in $(hm $((t_end - t_start))): $(gib "$used") GiB used; ${image_bytes:+image $(gib "$image_bytes") GiB; }report $out/README.md"
