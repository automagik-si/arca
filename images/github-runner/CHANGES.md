## What we change from GitHub's build, and what differs

Every script of GitHub's template runs unchanged, in GitHub's order, with GitHub's toolset file. These are the only
changes (`patch-template.py`, `vm/`), and each build stops when one of them no longer finds what it expects:

| GitHub's build | Ours | Why |
|---|---|---|
| Packer `azure-arm` source: an Azure `Standard_D4s_v4`, 75 GB disk, Canonical's Ubuntu server image | Packer `null` source over SSH to an Incus virtual machine: 4 CPUs, 8 GiB, 80 GiB ext4 root, Ubuntu cloud image from images.linuxcontainers.org | no Azure |
| Last step `waagent -force -deprovision+user` | `vm/finalize.sh`: removes the build user, cloud-init's state, the machine id, SSH host keys and the logs | no Azure agent |
| Canonical's Ubuntu server image as the base | the images.linuxcontainers.org cloud image plus Ubuntu's `ubuntu-server` and `ubuntu-standard` package sets, installed before GitHub's scripts | GitHub's scripts expect what the server image carries (motd-news, snapd, needrestart, fwupd) |
| `configure-environment.sh` edits `/etc/waagent.conf` | an empty `/etc/waagent.conf` before the build takes the edits | the script stops when the file is missing |
| A failed image test stops the build | recorded and the build goes on (record mode, the default); the failures are listed here | one failure must not cost a whole build; `--tests strict` behaves as GitHub |
| The software report and the final tests run in the build user's SSH session | the same step first reads `/etc/environment` | over our SSH session GitHub's PATH additions were missing there (`ansible` in `/opt/pipx_bin` not found) |
| Azure's agent makes 4096 MB of swap on the resource disk `/mnt` | `arca-swap.service` makes `/mnt/swapfile`, 4096 MB, at boot | the same swap without carrying the file in the image |
| GitHub's provisioning adds the `runner` user and the runner program to each machine | in the image: `runner` (uid 1001, passwordless sudo, groups adm, users, systemd-journal, docker), `/home/runner/work`, the runner in `/home/runner/runners/<version>`, GitHub's `/opt/post-generation` scripts run once | a job machine starts ready |
| Homebrew belongs to the build user | `/home/linuxbrew` belongs to `runner` | the build user is removed |
| The runner is registered by GitHub's own service | `arca-runner.service` runs one just-in-time runner per machine, its work folder set to `/home/runner/work`, then powers the machine off | one machine per job |

Known differences that stay:

- **Kernel**: GitHub runs Ubuntu's Azure kernel (`linux-azure`); ours is the kernel of the base image (Ubuntu's generic
  or virtual flavour of the same release). The software report's kernel line shows both.
- **Base packages**: GitHub starts from Canonical's Azure server image, we from the Ubuntu cloud image of
  images.linuxcontainers.org, so a few base packages (Azure's agent, cloud tools) differ. Everything GitHub's scripts
  and toolset install is the same.
- **Tools installed as "latest"**: some of GitHub's scripts take the newest release on the day of the build (the
  software comparison below lists every one that moved between their build and ours).
- **The machine**: CPU model, CPU count, memory and disk speed are those of the job class, not GitHub's.
- **`runner.environment`**: GitHub tells every job whether its runner is `github-hosted` or `self-hosted`; the server
  sets it, so ours is `self-hosted`. npm trusted publishing and provenance refuse self-hosted runners.
- **The runner's name and labels** are those of Arca's runner group, not GitHub's.
