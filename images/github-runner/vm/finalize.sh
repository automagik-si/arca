#!/bin/bash
# Run as root inside the build machine by build.sh once GitHub's template has finished. It does what GitHub's own
# machine provisioning does on top of their image, and what their last step (`waagent -deprovision+user`) did:
#
# - the `runner` user (uid 1001, passwordless sudo, groups adm, users, systemd-journal, docker), its /home/runner/work;
# - the runner program in /home/runner/runners/<version>, with the image's details in its .setup_info (shown at the top
#   of every job log as "Runner Image") and its .env/.path written by the runner's own env.sh from /etc/environment;
# - GitHub's post-generation scripts (/opt/post-generation), which they run on every machine made from the image;
# - arca-runner.service: at boot, wait for this machine's one just-in-time runner configuration, run it, power off;
# - a 4 GB swap file at boot, which Azure's agent makes on GitHub's machines;
# - then removes the build user, our build files, cloud-init's state, the machine id, SSH host keys and the logs.
#
# Environment: RUNNER_VERSION, RUNNER_SHA256, IMAGE_OS, IMAGE_VERSION, RUNNER_IMAGES_TAG, ARCA_DIFF_URL.
set -euo pipefail
: "${RUNNER_VERSION:?}" "${RUNNER_SHA256:?}" "${IMAGE_OS:?}" "${IMAGE_VERSION:?}" "${RUNNER_IMAGES_TAG:?}" "${ARCA_DIFF_URL:?}"
say() { printf 'finalize: %s\n' "$*"; }

# The build user (cloud-init made it, with its sudo rule) goes first, so `runner` is the last line of /etc/passwd:
# GitHub's post-generation scripts take the last user as the machine's user.
if id packer >/dev/null 2>&1; then
  loginctl terminate-user packer 2>/dev/null || true
  pkill -KILL -u packer 2>/dev/null || true
  userdel -r packer 2>/dev/null || userdel packer
fi
rm -f /etc/sudoers.d/90-cloud-init-users

say "runner user"
if ! id runner >/dev/null 2>&1; then
  useradd --create-home --uid 1001 --user-group --shell /bin/bash runner
fi
for group in adm users systemd-journal docker; do
  getent group "$group" >/dev/null && usermod -aG "$group" runner
done
printf 'runner ALL=(ALL) NOPASSWD:ALL\n' >/etc/sudoers.d/runner
chmod 0440 /etc/sudoers.d/runner
install -d -o runner -g runner /home/runner/work
# Homebrew was installed by the build user (as on GitHub, where Azure's build user does it); give it to `runner`, the
# only user left, so `brew install` works in a job.
[ -d /home/linuxbrew ] && chown -R runner:runner /home/linuxbrew

say "runner program $RUNNER_VERSION"
runner_dir=/home/runner/runners/$RUNNER_VERSION
tarball=/tmp/actions-runner.tar.gz
curl -fsSL --retry 5 -o "$tarball" \
  "https://github.com/actions/runner/releases/download/v$RUNNER_VERSION/actions-runner-linux-x64-$RUNNER_VERSION.tar.gz"
printf '%s  %s\n' "$RUNNER_SHA256" "$tarball" | sha256sum -c --quiet
install -d "$runner_dir"
tar -xzf "$tarball" -C "$runner_dir"
rm -f "$tarball"
# The runner's own dependency check; on this image everything it names is already there.
"$runner_dir/bin/installdependencies.sh" >/dev/null

# The "Runner Image" group GitHub shows at the top of every job, plus one line saying whose build this is.
jq --arg diff "$ARCA_DIFF_URL" --arg tag "$RUNNER_IMAGES_TAG" \
  '. + [{group: "Arca", detail: ("Built by Arca from actions/runner-images \($tag) with GitHub'"'"'s own scripts.\nDifferences from GitHub'"'"'s image: \($diff)")}]' \
  /imagegeneration/imagedata.json >"$runner_dir/.setup_info"
ln -sfn "$runner_dir" /home/runner/actions-runner
chown -R runner:runner /home/runner

say "post-generation scripts"
for script in /opt/post-generation/*.sh; do
  [ -e "$script" ] || continue
  say "  $(basename "$script")"
  bash "$script"
done

# The runner's .env and .path, written by its own env.sh in the environment a job gets (after post-generation, which
# puts the user's home into /etc/environment).
runuser -u runner -- env -i bash -c 'set -a; . /etc/environment; set +a; export HOME=/home/runner USER=runner;
  cd "$1" && ./env.sh >/dev/null' _ "$runner_dir"

say "arca-runner service and swap"
install -m 0755 /tmp/arca-image/arca-runner-start /usr/local/sbin/arca-runner-start
install -m 0644 /tmp/arca-image/arca-runner.service /etc/systemd/system/arca-runner.service
install -m 0644 /tmp/arca-image/arca-swap.service /etc/systemd/system/arca-swap.service
systemctl enable arca-runner.service arca-swap.service

say "clean-up, as waagent -deprovision+user does"
rm -rf /usr/local/lib/arca-image /tmp/arca-image
if command -v cloud-init >/dev/null; then
  cloud-init clean --logs --machine-id
fi
truncate -s 0 /etc/machine-id
rm -f /var/lib/dbus/machine-id /etc/ssh/ssh_host_*
rm -rf /root/.cache /root/.bash_history /tmp/* /var/tmp/*
find /var/log -type f -regex '.*\.\(gz\|[0-9]\)$' -delete
find /var/log -type f -exec truncate -s 0 {} +
sync
fstrim -av >/dev/null 2>&1 || true
say "done"
