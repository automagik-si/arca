#!/usr/bin/env bash
# Usage: bash templates/automagik.build.sh PROJECT PROFILE local:BASE_FINGERPRINT
# Run explicitly on a build host. PROJECT and PROFILE must already exist and provide
# storage and internet access. The immutable amd64 Debian base must include bash,
# ca-certificates, curl, git, xz-utils, tar, passwd, util-linux and systemd.
# Publishes only in PROJECT; no host packages, networks or units are changed.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project=${1:?expected project}
profile=${2:?expected profile}
base=${3:?expected local base image fingerprint}
[[ $project =~ ^[a-zA-Z0-9][a-zA-Z0-9_-]*$ && $profile =~ ^[a-zA-Z0-9][a-zA-Z0-9_-]*$ ]] || exit 2
[[ $base =~ ^local:[0-9a-f]{64}$ ]] || { echo 'base must be local:<64 hex fingerprint>' >&2; exit 2; }
# Read public template values without interpreting them as shell code.
mapfile -t pins < <(python3 - "$root/automagik.json" <<'PY'
import json, re, sys
with open(sys.argv[1]) as source:
    t = json.load(source)
assert re.fullmatch('[0-9a-f]{40}', t['commit'])
assert t['image'] == 'local:arca-automagik-' + t['commit']
assert isinstance(t['dashPort'], int) and 1 <= t['dashPort'] <= 65535
print(t['commit'])
print(t['image'][6:])
print(t['dashPort'])
PY
)
[[ ${#pins[@]} == 3 ]] || exit 2
commit=${pins[0]}
alias=${pins[1]}
port=${pins[2]}
builder=build-automagik
# Refuse an existing builder or alias. Cleanup touches only a builder this run made.
if incus info "$builder" --project "$project" >/dev/null 2>&1; then
  echo 'builder already exists' >&2; exit 1
fi
if incus image info "$alias" --project "$project" >/dev/null 2>&1; then
  echo 'image alias already exists' >&2; exit 1
fi
made=false
cleanup() {
  local status=$?
  if "$made"; then incus delete --force "$builder" --project "$project" || status=1; fi
  exit "$status"
}
trap cleanup EXIT
incus init "$base" "$builder" --project "$project" --profile "$profile"
made=true
for file in package.json package-lock.json; do
  incus file push --create-dirs --mode 0644 "$root/automagik-dsh/$file" "$builder/opt/arca-dsh/$file" --project "$project"
done
incus start "$builder" --project "$project"
incus exec "$builder" --project "$project" --force-noninteractive \
  --env AUTOMAGIK_COMMIT="$commit" --env DASH_PORT="$port" -- bash -s <<'INSTALL'
set -euo pipefail
[[ $(uname -m) == x86_64 ]] || { echo 'amd64 base required' >&2; exit 1; }
for program in curl git xz tar useradd runuser systemctl; do command -v "$program" >/dev/null; done
# The closed service exec table uses these exact system-template paths.
for program in /usr/bin/timeout /usr/bin/journalctl /usr/bin/systemctl /usr/bin/rm /usr/bin/mkdir; do test -x "$program"; done
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
cd "$scratch"
node_version=24.22.1
curl --fail --silent --show-error --location -o node.tar.xz "https://nodejs.org/dist/v$node_version/node-v$node_version-linux-x64.tar.xz"
printf '%s  node.tar.xz\n' '6c172a9ab6718f7b801fb5a7bd486af2ee5e32758086a04b84fcce2aa77095e6' | sha256sum --check --status
tar -xJf node.tar.xz -C /usr/local --strip-components=1 --no-same-owner
cd /opt/arca-dsh
npm ci --ignore-scripts --omit=dev --no-audit --no-fund
chmod 0755 node_modules/node-pty/prebuilds/linux-x64/spawn-helper
node -e 'require("./node_modules/node-pty")'
ln -s /opt/arca-dsh/node_modules/.bin/dsh /usr/local/bin/dsh
ln -s /opt/arca-dsh/node_modules/.bin/pnpm /usr/local/bin/pnpm
cd "$scratch"
ts_version=1.104.2
curl --fail --silent --show-error --location -o tailscale.tgz "https://pkgs.tailscale.com/stable/tailscale_${ts_version}_amd64.tgz"
printf '%s  tailscale.tgz\n' '512f613ed7d04fc7282f7dc5bd76aa105aae28168fbb9357eb749ebcbc29f0ae' | sha256sum --check --status
tar -xzf tailscale.tgz
install -m 0755 "tailscale_${ts_version}_amd64/tailscale" /usr/bin/tailscale
test -x /usr/bin/tailscale
install -m 0755 "tailscale_${ts_version}_amd64/tailscaled" /usr/sbin/tailscaled
install -m 0644 "tailscale_${ts_version}_amd64/systemd/tailscaled.service" /etc/systemd/system/tailscaled.service
install -m 0644 "tailscale_${ts_version}_amd64/systemd/tailscaled.defaults" /etc/default/tailscaled
useradd --create-home --shell /bin/bash automagik
cd /home/automagik
as_user() { runuser -u automagik -- env HOME=/home/automagik DSH_TELEMETRY_DISABLED=1 "$@"; }
as_user dsh plugin --profile web add -w --ignore-scripts \
  "git+https://github.com/automagik-dev/automagik.git#$AUTOMAGIK_COMMIT&path:/apps/bots-dsh" \
  "git+https://github.com/automagik-dev/automagik.git#$AUTOMAGIK_COMMIT&path:/apps/welcome-dsh" \
  'git+https://github.com/automagik-dev/omni.git#45a6832d8117efd8fc07b434722d55ea7f034902&path:/apps/omni-channels-dsh'
# Refuse dependency drift, including a plugin gaining a new transitive dependency.
node <<'JS'
const fs = require('node:fs');
const yaml = require('/opt/arca-dsh/node_modules/yaml');
const profile = '/home/automagik/.dsh/profiles/web/';
const settings = yaml.parse(fs.readFileSync(profile + 'pnpm-workspace.yaml', 'utf8'));
if (settings.dangerouslyAllowAllBuilds || settings.onlyBuiltDependencies ||
    Object.values(settings.allowBuilds ?? {}).some(v => v !== false && v !== 'set this to true or false')) throw Error('build scripts allowed');
const commits = new Map([
  ['@automagik/bots-dsh', process.env.AUTOMAGIK_COMMIT],
  ['@automagik/welcome-dsh', process.env.AUTOMAGIK_COMMIT],
  ['@automagik/omni-channels-dsh', '45a6832d8117efd8fc07b434722d55ea7f034902'],
]);
const lock = yaml.parse(fs.readFileSync(profile + 'pnpm-lock.yaml', 'utf8'));
const seen = new Set();
for (const [key, pkg] of Object.entries(lock.packages ?? {})) {
  const name = key.slice(0, key.indexOf('@', 1));
  const valid = name === 'zod'
    ? key === 'zod@4.6.5' && pkg.resolution?.integrity === 'sha512-v5l/aFXZQeai4awLbOpSoHecE9UiMrnfx75tEXLjNonXVARxQ5mOeipTjROUchszUNCqnE+hqAMujRsRHsut2Q=='
    : commits.has(name) && JSON.stringify([key, pkg.resolution]).includes(commits.get(name));
  if (!valid || seen.has(name)) throw Error('plugin lock differs from pins');
  seen.add(name);
}
if (seen.size !== 4) throw Error('plugin missing from lock');
JS
# Keep the launch token in a root-only runtime file, never in the console/journal.
cat > /usr/local/bin/automagik-dash <<'DASH'
#!/bin/bash
set -uo pipefail
umask 077
install -d -m 0700 /run/automagik-dash
rm -f /run/automagik-dash/url
runuser -u automagik -- env HOME=/home/automagik DSH_TELEMETRY_DISABLED=1 /usr/local/bin/dsh --profile web --no-open --host 127.0.0.1 --port "$1" 2>&1 |
while IFS= read -r line; do
  if [[ $line =~ token=([A-Za-z0-9_-]+) ]]; then
    if [[ $line == 'dsh web: http'* ]]; then
      printf '%s\n' "${line#dsh web: }" > /run/automagik-dash/url
      line='dsh web: ready'
    else
      line=${line//${BASH_REMATCH[1]}/[secret]}
    fi
  fi
  printf '%s\n' "$line"
done
exit "${PIPESTATUS[0]}"
DASH
chmod 0755 /usr/local/bin/automagik-dash
cat > /etc/systemd/system/automagik-dash.service <<UNIT
[Unit]
Description=Automagik dashboard
After=network-online.target
Wants=network-online.target
[Service]
WorkingDirectory=/home/automagik
ExecStart=/usr/local/bin/automagik-dash $DASH_PORT
Restart=on-failure
RestartSec=5
[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable automagik-dash.service
# tailscaled is installed but remains disabled until publication is requested.
systemctl disable tailscaled.service
printf '{"commit":"%s","dashPort":%s}\n' "$AUTOMAGIK_COMMIT" "$DASH_PORT" > /etc/arca-automagik.json
truncate -s 0 /etc/machine-id
INSTALL
incus stop "$builder" --project "$project"
incus publish "$builder" --project "$project" --alias "$alias" \
  "description=Automagik at $commit" "arca.commit=$commit" "arca.base=${base#local:}"
