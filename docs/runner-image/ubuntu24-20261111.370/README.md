# Arca's GitHub runner image, ubuntu24/20261111.370

Built from GitHub's own scripts in [actions/runner-images ubuntu24/20261111.370](https://github.com/actions/runner-images/releases/tag/ubuntu24%2F20261111.370) (commit `fb099341ddaf44a428fb17495341cb7c2300faf2`) by `images/github-runner/build.sh ubuntu24`.

| | |
|---|---|
| ImageOS, ImageVersion | `ubuntu24`, `20261111.370.1` (the same as GitHub's image of this release) |
| Runner program | 2.338.2, in /home/runner/runners/2.338.2 |
| Built on | 2026-11-14, a virtual machine of 4 CPUs and 8GiB, root disk throttled to 150MB/s read and 80MB/s write; Packer 1.16.2; base images:ubuntu/24.04/cloud |
| Build time | 1h35m in all: machine 0h05m, GitHub's template 1h29m, report and finish 0h00m, image 0h00m |
| Size | 54 GiB used on the root disk |
| Result | the stopped machine arca-runner-build-ubuntu24, snapshot image, in project ci |
| GitHub's image tests | all passed |

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

## Software: ours against GitHub's image of the same release

Made with GitHub's comparer (`helpers/software-report-base/Calculate-ImagesDifference.ps1`): GitHub's report (`internal.ubuntu24.json` of the release) is the previous one, ours the current one. Tools GitHub's scripts install as "latest" can move between their build day and ours.

# :desktop_computer: Actions Runner Image: Ubuntu 24.04
- OS Version: 24.04.5 LTS
- Kernel Version: 6.8.0-149-generic
- Image Version: 20261111.370.1
- Systemd version: 255.4-1ubuntu8.18

## :mega: What's changed?

### Updated

<table>
    <thead>
        <th>Category</th>
        <th>Tool name</th>
        <th>Previous (20261111.370.1)</th>
        <th>Current (20261111.370.1)</th>
    </thead>
    <tbody>
        <tr>
            <td rowspan="1"></td>
            <td>Kernel Version</td>
            <td>7.0.0-1017-azure</td>
            <td>6.8.0-149-generic</td>
        </tr>
        <tr>
            <td rowspan="2">Language and Runtime</td>
            <td>Julia</td>
            <td>1.13.3</td>
            <td>1.13.4</td>
        </tr>
        <tr>
            <td>Node.js</td>
            <td>22.23.4</td>
            <td>22.23.5</td>
        </tr>
        <tr>
            <td rowspan="1">Package Management</td>
            <td>Vcpkg</td>
            <td>(build from commit 2ec0f1747d)</td>
            <td>(build from commit 4702e381db)</td>
        </tr>
        <tr>
            <td rowspan="2">Tools</td>
            <td>Docker-Buildx</td>
            <td>0.38.2</td>
            <td>0.39.0</td>
        </tr>
        <tr>
            <td>Pulumi</td>
            <td>3.274.1</td>
            <td>3.276.0</td>
        </tr>
        <tr>
            <td rowspan="2">CLI Tools</td>
            <td>AWS CLI</td>
            <td>2.40.0</td>
            <td>2.40.3</td>
        </tr>
        <tr>
            <td>GitHub CLI</td>
            <td>2.103.3</td>
            <td>2.104.0</td>
        </tr>
        <tr>
            <td rowspan="1">PHP Tools</td>
            <td>PHPUnit</td>
            <td>8.5.56</td>
            <td>8.5.57</td>
        </tr>
        <tr>
            <td rowspan="3">Rust Tools</td>
            <td>Cargo</td>
            <td>1.99.1</td>
            <td>1.100.0</td>
        </tr>
        <tr>
            <td>Rust</td>
            <td>1.99.1</td>
            <td>1.100.0</td>
        </tr>
        <tr>
            <td>Rustdoc</td>
            <td>1.99.1</td>
            <td>1.100.0</td>
        </tr>
        <tr>
            <td rowspan="1">Rust Tools ><br> Packages</td>
            <td>Rustfmt</td>
            <td>1.10.0</td>
            <td>1.11.0</td>
        </tr>
        <tr>
            <td rowspan="4">Browsers and Drivers</td>
            <td>Google Chrome</td>
            <td>157.0.8103.50</td>
            <td>157.0.8103.78</td>
        </tr>
        <tr>
            <td>ChromeDriver</td>
            <td>157.0.8103.50</td>
            <td>157.0.8103.78</td>
        </tr>
        <tr>
            <td>Microsoft Edge</td>
            <td>157.0.4341.46</td>
            <td>157.0.4341.60</td>
        </tr>
        <tr>
            <td>Microsoft Edge WebDriver</td>
            <td>157.0.4341.46</td>
            <td>157.0.4341.60</td>
        </tr>
        <tr>
            <td rowspan="1">Databases ><br> PostgreSQL</td>
            <td>PostgreSQL</td>
            <td>16.15</td>
            <td>16.16</td>
        </tr>
        <tr>
            <td rowspan="1">Databases ><br> MySQL</td>
            <td>MySQL</td>
            <td>8.0.46-0ubuntu0.24.04.4</td>
            <td>8.0.46-0ubuntu0.24.04.5</td>
        </tr>
        <tr>
            <td rowspan="1">Cached Tools</td>
            <td>Node.js</td>
            <td>22.23.4, 24.22.2</td>
            <td>22.23.5, 24.22.3</td>
        </tr>
    </tbody>
</table>

#### Installed apt packages
| Name        | Version             |
| ----------- | ------------------- |
| ~~locales~~ | ~~2.39-0ubuntu8.9~~ |
| locales     | 2.39-0ubuntu8.12    |



Our full software list is [software-report.md](software-report.md) (GitHub's generator, run in our image).

## Notes on this build

- GitHub's image tests: 505 passed, 0 failed, 22 skipped (their own count in the final run).
- A machine made from the image was booted and checked: systemd running with no failed unit, only the `runner` user,
  `arca-runner.service` waiting for its configuration, 4 GB swap, Docker 28.0.4, the toolcache in place.
- Three finishing steps were found by that check and added to `vm/finalize.sh` after this build; they were applied to
  this image by hand the same way: no cloud-init default user and no build seed left in the image, GitHub's Azure
  resource-disk mount setting removed, and Ubuntu's LXD agent loader masked.
- The image was kept as the stopped machine with a snapshot, not published: publishing needs about twice its size free
  under `/var/lib/incus` on the build host.
