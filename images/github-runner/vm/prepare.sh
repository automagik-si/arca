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
echo "prepare: ready (root on $root_fs, $(df -h --output=size / | tail -1 | tr -d ' ') disk, $(nproc) CPUs)"
