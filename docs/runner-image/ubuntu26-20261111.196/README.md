# Arca's GitHub runner image, ubuntu26/20261111.196

Built from GitHub's own scripts in [actions/runner-images ubuntu26/20261111.196](https://github.com/actions/runner-images/releases/tag/ubuntu26%2F20261111.196) (commit `93a4e4e98715026b60fdf86893cf193b4be3bd47`) by `images/github-runner/build.sh ubuntu26`.

| | |
|---|---|
| ImageOS, ImageVersion | `ubuntu26`, `20261111.196.1` (the same as GitHub's image of this release) |
| Runner program | 2.338.2, in /home/runner/runners/2.338.2 |
| Built on | 2026-11-15, a virtual machine of 4 CPUs and 7GiB, root disk throttled to 150MB/s read and 80MB/s write; Packer 1.16.2; base images:ubuntu/26.04/cloud |
| Build time | 1h08m in all: machine 0h03m, GitHub's template 1h02m, report and finish 0h00m, image 0h01m |
| Size | 52 GiB used on the root disk |
| Result | the stopped machine arca-runner-image-ubuntu26-20261111-196, snapshot image, in project ci |
| GitHub's image tests | all passed |

## What we change from GitHub's build, and what differs

Every script of GitHub's template runs unchanged, in GitHub's order, with GitHub's toolset file. These are the only
changes (`patch-template.py`, `vm/`), and each build stops when one of them no longer finds what it expects:

| GitHub's build | Ours | Why |
|---|---|---|
| Packer `azure-arm` source: an Azure `Standard_D4s_v4`, 75 GB disk, Canonical's Ubuntu server image | Packer `null` source over SSH to an Incus virtual machine: 4 CPUs, 8 GiB, 80 GiB ext4 root, Ubuntu cloud image from images.linuxcontainers.org | no Azure |
| Last step `waagent -force -deprovision+user` | `vm/finalize.sh`: removes the build user, cloud-init's state, the machine id, SSH host keys and the logs | no Azure agent |
| Canonical's Ubuntu server image as the base | the images.linuxcontainers.org cloud image plus Ubuntu's `ubuntu-server` and `ubuntu-standard` package sets, installed before GitHub's scripts | GitHub's scripts expect what the server image carries (motd-news, snapd, needrestart, fwupd) |
| Ubuntu 26.04 keeps `/tmp` in memory until GitHub's reboot near the end (their build machine has 16 GB) | `/tmp` on the disk from the start, as GitHub's own `systemctl mask tmp.mount` leaves it | the toolset unpacks gigabytes in `/tmp`; half of an 8 GiB machine is not enough |
| `configure-environment.sh` edits `/etc/waagent.conf` | an empty `/etc/waagent.conf` before the build takes the edits | the script stops when the file is missing |
| A failed image test stops the build | recorded and the build goes on (record mode, the default); the failures are listed here | one failure must not cost a whole build; `--tests strict` behaves as GitHub |
| The software report and the final tests run in the build user's SSH session | the same step first reads `/etc/environment` | over our SSH session GitHub's PATH additions were missing there (`ansible` in `/opt/pipx_bin` not found) |
| Azure's agent makes 4096 MB of swap on the resource disk `/mnt` | `arca-swap.service` makes `/mnt/swapfile`, 4096 MB, at boot | the same swap without carrying the file in the image |
| GitHub's provisioning adds the `runner` user and the runner program to each machine | in the image: `runner` (uid 1001, passwordless sudo, groups adm, users, systemd-journal, docker), `/home/runner/work`, the runner in `/home/runner/runners/<version>`, GitHub's `/opt/post-generation` scripts run once | a job machine starts ready |
| A cloud-init setting mounts Azure's resource disk `ephemeral0` on `/mnt`; Ubuntu's LXD agent loader is installed | the setting is removed and `lxd-agent.service` is masked | neither exists outside Azure; both left a failed unit |
| cloud-init's default user | none (`users: []`); the build's cloud-init seed is removed | GitHub's machines have only `runner` |
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

Made with GitHub's comparer (`helpers/software-report-base/Calculate-ImagesDifference.ps1`): GitHub's report (`internal.ubuntu26.json` of the release) is the previous one, ours the current one. Tools GitHub's scripts install as "latest" can move between their build day and ours.

# :desktop_computer: Actions Runner Image: Ubuntu 26.04
- OS Version: 26.04.1 LTS
- Kernel Version: 7.0.0-43-generic
- Image Version: 20261111.196.1
- Systemd version: 259.9-0ubuntu0.1

## :mega: What's changed?

### Updated

<table>
    <thead>
        <th>Category</th>
        <th>Tool name</th>
        <th>Previous (20261111.196.1)</th>
        <th>Current (20261111.196.1)</th>
    </thead>
    <tbody>
        <tr>
            <td rowspan="1"></td>
            <td>Kernel Version</td>
            <td>7.0.0-1019-azure</td>
            <td>7.0.0-43-generic</td>
        </tr>
        <tr>
            <td rowspan="1">Language and Runtime</td>
            <td>Node.js</td>
            <td>24.22.2</td>
            <td>24.22.3</td>
        </tr>
        <tr>
            <td rowspan="2">Package Management</td>
            <td>Helm</td>
            <td>4.3.1</td>
            <td>4.3.2</td>
        </tr>
        <tr>
            <td>Vcpkg</td>
            <td>(build from commit 2ec0f1747d)</td>
            <td>(build from commit 4702e381db)</td>
        </tr>
        <tr>
            <td rowspan="2">Tools</td>
            <td>CMake</td>
            <td>4.4.5</td>
            <td>4.5.0</td>
        </tr>
        <tr>
            <td>Docker-Buildx</td>
            <td>0.38.2</td>
            <td>0.39.0</td>
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
            <td>18.6</td>
            <td>18.7</td>
        </tr>
        <tr>
            <td rowspan="1">Databases ><br> MySQL</td>
            <td>MySQL</td>
            <td>8.4.11-0ubuntu0.26.04.1</td>
            <td>8.4.13-0ubuntu0.26.04.2</td>
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
| ~~locales~~ | ~~2.43-2ubuntu2.4~~ |
| locales     | 2.43-2ubuntu2.7     |



Our full software list is [software-report.md](software-report.md) (GitHub's generator, run in our image).

## Notes on this build

- GitHub's image tests: 490 passed, 0 failed, 23 skipped (their own count in the final run).
- Built with 7 GiB of memory instead of 8: the project's memory quota counts the stopped 24.04 build machine too.
- A machine made from the image was booted and checked: systemd running with no failed unit, only the `runner` user,
  `arca-runner.service` waiting for its configuration, 4 GB swap, Docker 29.4.2 (GitHub's version for this release),
  `/tmp` on the disk, the toolcache in place.
- That check found that the build's pinned address and its disk throttle went with the snapshot (a copy would not
  start: its address was taken). `build.sh` now removes both before the snapshot; this image's snapshot was made again
  without them.
- The image was kept as the stopped machine with a snapshot, not published: publishing needs about twice its size free
  under `/var/lib/incus` on the build host.
- Its snapshot was made a third time without the build's NIC, which the address pin had copied from the profile into
  the machine (a copy would have kept it over its own profile's NIC).
