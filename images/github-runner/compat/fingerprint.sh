#!/bin/bash
# What a job sees on the runner, as "check<TAB>value" lines on standard output: run in the same job on GitHub's
# runner and on ours, then compare.sh puts the two side by side. A value is what a workflow depends on (a version, a
# path, whether something works), never what is expected to differ by machine (CPU count, memory, host name, run ids).
#
#   fingerprint.sh <group>     groups: environment, tools, system, toolcache
set -u
group=${1:?usage: fingerprint.sh <environment|tools|system|toolcache>}
row() { printf '%s\t%s\n' "$1" "$2"; }
# The first line of a command's output, or "missing"/"failed".
first() {
  local name=$1 out
  shift
  if ! command -v "$1" >/dev/null 2>&1; then
    row "$name" missing
    return
  fi
  if out=$("$@" 2>&1 </dev/null); then
    row "$name" "$(printf '%s\n' "$out" | grep -m1 . | tr '\t' ' ')"
  else
    row "$name" "failed: $(printf '%s\n' "$out" | grep -m1 . | tr '\t' ' ')"
  fi
}

case $group in
  environment)
    for v in ImageOS ImageVersion RUNNER_TOOL_CACHE AGENT_TOOLSDIRECTORY ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE \
      HOME USER RUNNER_TEMP GITHUB_WORKSPACE RUNNER_WORKSPACE XDG_CONFIG_HOME ACCEPT_EULA DEBIAN_FRONTEND \
      JAVA_HOME JAVA_HOME_17_X64 JAVA_HOME_21_X64 ANDROID_HOME ANDROID_NDK_HOME CHROMEWEBDRIVER GECKOWEBDRIVER \
      EDGEWEBDRIVER SELENIUM_JAR_PATH CONDA VCPKG_INSTALLATION_ROOT PIPX_HOME PIPX_BIN_DIR GOROOT_1_25_X64 \
      SWIFT_PATH BOOTSTRAP_HASKELL_NONINTERACTIVE GHCUP_INSTALL_BASE_PREFIX POWERSHELL_DISTRIBUTION_CHANNEL; do
      row "env $v" "${!v-<unset>}"
    done
    # PATH, one entry per row, so a missing entry shows as one row.
    i=0
    IFS=: read -ra parts <<<"$PATH"
    for p in "${parts[@]}"; do
      i=$((i + 1))
      row "PATH $(printf '%02d' "$i")" "$p"
    done
    row "whoami" "$(whoami)"
    row "id -nG" "$(id -nG | tr ' ' '\n' | sort | tr '\n' ' ')"
    row "id -u" "$(id -u)"
    row "sudo -n true" "$(sudo -n true 2>/dev/null && echo ok || echo refused)"
    row "pwd" "$(pwd)"
    row "shell" "$(getent passwd "$(whoami)" | cut -d: -f7)"
    ;;
  tools)
    first zstd zstd --version
    first libatomic1 dpkg-query -W -f '${Status}' libatomic1
    first node node --version
    first npm npm --version
    first yarn yarn --version
    first pnpm pnpm --version
    first corepack corepack --version
    first uv uv --version
    first gh gh --version
    first skopeo skopeo --version
    first wget wget --version
    first curl curl --version
    first git git --version
    first git-lfs git-lfs --version
    first docker docker --version
    first "docker server" docker version --format '{{.Server.Version}}'
    first "docker compose" docker compose version
    first "docker buildx" docker buildx version
    first podman podman --version
    first buildah buildah --version
    first python3 python3 --version
    first pip3 pip3 --version
    first pipx pipx --version
    first go go version
    first java java -version
    first dotnet dotnet --version
    first ruby ruby --version
    first php php --version
    first rustc rustc --version
    first cargo cargo --version
    first gcc gcc --version
    first g++ g++ --version
    first clang clang --version
    first cmake cmake --version
    first make make --version
    first ninja ninja --version
    first kubectl kubectl version --client
    first helm helm version --short
    first kind kind --version
    first minikube minikube version --short
    first terraform terraform version
    first az az version --output tsv
    first aws aws --version
    first gcloud gcloud --version
    first pwsh pwsh --version
    first jq jq --version
    first yq yq --version
    first google-chrome google-chrome --version
    first chromedriver chromedriver --version
    first firefox firefox --version
    first microsoft-edge microsoft-edge --version
    first mysql mysql --version
    first psql psql --version
    first sqlite3 sqlite3 --version
    first brew brew --version
    first snap snap --version
    first xvfb-run sh -c 'command -v xvfb-run'
    first 7z 7z i
    first zip zip -v
    first unzip unzip -v
    first xz xz --version
    first rsync rsync --version
    first shellcheck shellcheck --version
    ;;
  system)
    row "os" "$(. /etc/os-release && echo "$PRETTY_NAME")"
    row "kernel flavour" "$(uname -r | sed 's/^[0-9.-]*//')"
    row "init" "$(cat /proc/1/comm)"
    row "virtualisation" "$(systemd-detect-virt 2>/dev/null | sed 's/^none$/none/' || echo unknown)"
    row "root filesystem" "$(findmnt -no FSTYPE /)"
    for s in vm.max_map_count fs.inotify.max_user_watches fs.inotify.max_user_instances vm.mmap_rnd_bits; do
      row "sysctl $s" "$(sysctl -n "$s" 2>/dev/null || echo unreadable)"
    done
    row "ulimit -n" "$(ulimit -n)"
    row "ulimit -s" "$(ulimit -s)"
    row "swap" "$(swapon --show=SIZE --noheadings --bytes 2>/dev/null | awk '{s+=$1} END {print (s >= 4000000000 ? "4 GB or more" : (s > 0 ? "under 4 GB" : "none"))}')"
    row "/etc/hosts ::1" "$(grep -m1 '^::1' /etc/hosts | tr -s ' \t' ' ')"
    row "/opt writable" "$(test -w /opt && echo yes || echo no)"
    row "/usr/share writable" "$(test -w /usr/share && echo yes || echo no)"
    row "/mnt" "$(test -d /mnt && echo present || echo missing)"
    for unit in docker mysql postgresql nginx apache2 snapd; do
      row "service $unit" "$(systemctl is-enabled "$unit" 2>/dev/null || echo absent)/$(systemctl is-active "$unit" 2>/dev/null)"
    done
    for p in libnss3 libgbm1 libasound2t64 libatk-bridge2.0-0t64 libgtk-3-0t64 libxss1 fonts-liberation libatomic1 \
      zstd xvfb build-essential libssl-dev; do
      row "package $p" "$(dpkg-query -W -f '${Status}' "$p" 2>/dev/null | awk '{print $3}' || echo absent)"
    done
    row "apt-get install" "$(sudo apt-get install -y -q cowsay >/dev/null 2>&1 && echo ok || echo failed)"
    ;;
  toolcache)
    # What is prefilled, by tool and version folder (complete ones only), and the action archive cache.
    if [ -d "${RUNNER_TOOL_CACHE:-/nonexistent}" ]; then
      for tool in "$RUNNER_TOOL_CACHE"/*/; do
        name=$(basename "$tool")
        versions=$(find "$tool" -mindepth 2 -maxdepth 2 -name 'x64.complete' -printf '%h\n' 2>/dev/null |
          xargs -r -n1 basename | sort -V | tr '\n' ' ')
        row "toolcache $name" "${versions:-none}"
      done
    else
      row "toolcache" "missing"
    fi
    row "action archive cache" "$(find "${ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE:-/nonexistent}" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l) entries"
    ;;
  *)
    echo "fingerprint.sh: unknown group $group" >&2
    exit 2
    ;;
esac
