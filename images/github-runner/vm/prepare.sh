#!/bin/bash
# Run as root inside the build machine by build.sh, before Packer. It puts in place what GitHub's scripts expect from
# an Azure machine and our record-mode test helper. Nothing here stays in the image (finalize.sh removes it).
set -euo pipefail

# GitHub's scripts refuse anything but ext4 (configure-environment.sh); say so before hours of work, not after.
root_fs=$(findmnt --noheadings --first-only --output FSTYPE --target /)
if [ "$root_fs" != ext4 ]; then
  echo "prepare: the root filesystem is $root_fs; GitHub's scripts need ext4" >&2
  exit 1
fi

# GitHub starts from Canonical's Ubuntu server image; the cloud image here is smaller. Ubuntu's own server and standard
# package sets bring what GitHub's scripts take for granted (motd-news, snapd, needrestart, fwupd and the rest).
echo "prepare: installing ubuntu-server and ubuntu-standard, as on Canonical's server image"
export DEBIAN_FRONTEND=noninteractive
apt-get update -q >/dev/null
apt-get install -y -q ubuntu-server ubuntu-standard >/dev/null

# configure-environment.sh edits the Azure agent's settings and stops when the file is missing. There is no agent:
# an empty file takes the edits and does nothing.
[ -e /etc/waagent.conf ] || : >/etc/waagent.conf

# cloud-init leaves the build user's password locked ("!"), and Ubuntu's sshd then refuses its key too. "*" means no
# password at all, which sshd accepts for a key.
usermod -p '*' packer

install -d -m 0755 /usr/local/lib/arca-image
install -m 0755 /tmp/arca-image/invoke-tests-record.sh /usr/local/lib/arca-image/invoke-tests-record.sh
: >/usr/local/lib/arca-image/test-failures.log
chmod 0666 /usr/local/lib/arca-image/test-failures.log
# Ubuntu 26.04 keeps /tmp in memory (half the machine's); GitHub's toolset unpacks gigabytes there and only turns it off
# (configure-environment.sh masks tmp.mount) for after the reboot near the end. Put /tmp on the disk now, the way it
# ends up in GitHub's image. Last here: the files build.sh put in /tmp are already installed.
if [ "$(findmnt -n -o FSTYPE /tmp 2>/dev/null)" = tmpfs ]; then
  systemctl mask tmp.mount
  umount -l /tmp
  chmod 1777 /tmp
  echo "prepare: /tmp moved from memory to the disk"
fi
echo "prepare: ready (root on $root_fs, $(df -h --output=size / | tail -1 | tr -d ' ') disk, $(nproc) CPUs)"
