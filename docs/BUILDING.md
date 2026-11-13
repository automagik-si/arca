# Building and releasing

## Build and test

The toolchain is pinned in `rust-toolchain.toml`.

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

A plain `cargo build --release` needs the build machine's glibc or newer on the host that runs the binary.
`scripts/build-portable.sh` builds `arca` against glibc 2.34 (Enterprise Linux 9 and newer) with cargo-zigbuild and zig;
both are pinned, checked by SHA-256 and kept under `~/.cache/arca-portable-tools` (or `ARCA_PORTABLE_TOOLS`), with no
root. It runs on x86_64 or aarch64 Linux, and builds for the machine it runs on. It needs `curl`, `xz`, `sha256sum` and
`objdump`.
It prints the zig it used, the binary's path, its SHA-256 and the highest GLIBC symbol version the binary needs, and fails when that version is above 2.34 or cannot be read.
It also fails when the binary names a path of the build machine (under the home directory, cargo's home, the checkout, the build's own directories, the toolchain or the tools), and lists the paths it found. A line of a committed file the binary is built from is not such a path. `scripts/build-portable.sh --check-paths <file>` runs that check alone, against the same directories.

## Releasing

Nobody writes a version or types a tag. Every push to `dev` starts the release workflow
([`.github/workflows/release.yml`](../.github/workflows/release.yml)), which cuts a release by itself when something that
ships has changed:

1. It runs the gates: `cargo fmt`, `cargo clippy`, the tests, and the canvas package's tests.
2. It works out the version from the release tags (`scripts/version.sh`). The whole repository carries one version,
   `0.N.P`. A change to anything the binary is built from is a full release, `0.N.0`, with `N` one above the last. A
   change that ships without changing the binary (the canvas package, the units, the schema) is a patch,
   `0.N.(P+1)`. A change to documents, tests or the workflows alone releases nothing. The lists are at the top of
   `scripts/version.sh`.
3. It writes the version into the manifests, the lock files and the installer (the `ARCA_VERSION` line of
   `scripts/install.sh`), writes that version's section of `CHANGELOG.md`, commits them as
   `chore(release): v<version>`, tags the commit `v<version>`, and builds the release files at the tag with
   `scripts/release.sh`.
4. A second job pushes `dev` and the tag together and publishes the GitHub release with
   `arca-<version>-linux-x86_64`, `arca-<version>-linux-aarch64` (built by its own job on GitHub's arm64 runner, with
   the same script), `install.sh` (the installer as the release commit holds it, which installs that release) and
   `SHA256SUMS`, the checksums of those three.
5. `main` moves to the release commit. `main` holds released commits and nothing else.
   GitHub refuses that push when the release carries a change to a workflow file: the job's own token may not move a
   branch across one, and no other credential is kept for it. The release is already published by then; the job
   fails and prints the one command that moves `main`, for someone whose own access has the workflow scope.

Steps 1 to 3 run the repository's code and its dependencies' (build scripts, tests, a package's install), so their
job may only read: it holds no token that can write. It hands the release commit, its tag and the files to the second
job, which holds the token that can write and runs none of that code. Before it pushes, the second job checks the
release commit against `dev`'s own copy of each file, and the list is all there is:

- one commit on top of `dev`, whose whole message is `chore(release): v<version>`, with an annotated tag of that name
  whose whole message is `Automagik Arca <version>`;
- six files changed and no other, none changing its kind or mode;
- in the manifests and lock files, only the version lines of the workspace's crates and of the canvas package, from
  the old version to the new one. A manifest or a lock file can carry code, so nothing else in them may change in a
  commit nobody reviews;
- in `scripts/install.sh`, only its one `ARCA_VERSION` line, from the old version to the new one. Every other byte
  of the installer is `dev`'s own;
- in `CHANGELOG.md`, no line removed or changed, and the added lines being the one heading, right under `Unreleased`
  and outside any comment, and blank lines and list lines (`- ...`) inside that section, with no control character
  and no `<`;
- the release files being the four expected; `SHA256SUMS` holding two lines, the binary's and then `install.sh`'s,
  written as `sha256sum` writes them (the sum, two spaces, the name), and both files matching it; and `install.sh` being, byte for byte, the release commit's `scripts/install.sh`, whose
  `ARCA_VERSION` line names the version being released. An installer at another version stops the release.

What this does not stop: code that runs in the first job, a hostile dependency among it, can still shape the binary
that job builds, the words of the changelog's list lines and of the release notes (a line may hold a link), and which
version number the release takes. The split keeps such code away from `dev`, `main` and every release already
published; it does not make the new release's binary trustworthy by itself.

If any step before the push fails, no version is taken. If the push landed and the publishing failed, the next run
publishes that same tag before it does anything else; start one by hand with `gh workflow run release.yml --ref dev`.
Add `--field prerelease=true` to publish a release that is not shown as Latest. One run works at a time and GitHub
keeps a single waiting run: a waiting run is replaced by a newer one. For a push that loses nothing, because the newer
run releases everything since the last tag. A waiting dispatch with `prerelease=true` that a push's run replaces did
not happen, and that run publishes a normal release: dispatch a prerelease when no run is waiting or in progress, and
look at the run afterwards.

**What a contributor writes.** Commit subjects in the form `type(scope): what changed`, in plain words: the release
lists them in `CHANGELOG.md`, one line per change that landed on `dev`. A change that needs more than its line gets a
few sentences under `## Unreleased` in `CHANGELOG.md`; the release puts its heading above them, so they lead its section. Nothing else:
no version, no tag, no heading.

After each release `dev` on GitHub is one commit ahead of a local `dev`, by the release commit: fast-forward before
the next push.

The release commit has no CI run of its own: it is pushed with the workflow's own token, and GitHub starts no workflow
for such a push. The cut's own gate covers it. It ran on the commit's parent, and the release commit adds only the
version and the changelog section. For the same reason the push stays on the workflow's token: with a personal or an
app token the release commit would start the workflows again. The release workflow always runs on GitHub's hosted
runners, never on a self-hosted one, whose kept cache beside a write token would give a pull request a way to write
`dev`.

`scripts/release.sh` is the one way the release files are built, and it publishes nothing. From a clean tree whose
HEAD is the tag `v<version>` it runs `scripts/build-portable.sh`, checks that the binary's `--version` names that
version and commit, and writes `dist/arca-<version>-linux-<architecture>` (the machine's own: x86_64 or aarch64),
`dist/install.sh` (a copy of `scripts/install.sh`), `dist/SHA256SUMS` (the checksums of those two) and
`dist/RELEASE-NOTES.md` (that version's section of `CHANGELOG.md`). It refuses, before it builds, an installer whose
`ARCA_VERSION` line does not name the version. `scripts/release.sh --rehearse` builds and checks the same files without
the tag, to try a build by hand.
