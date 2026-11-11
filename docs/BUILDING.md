# Building and releasing

## Build and test

The toolchain is pinned in `rust-toolchain.toml`.

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

A plain `cargo build --release` needs the build machine's glibc or newer on the host that runs the binary.
`scripts/build-portable.sh` builds `arca` against glibc 2.34 (Enterprise Linux 9 and newer) with cargo-zigbuild and zig; both are pinned, checked by SHA-256 and kept under `~/.cache/arca-portable-tools` (or `ARCA_PORTABLE_TOOLS`), with no root. It runs on x86_64 Linux and needs `curl`, `xz`, `sha256sum` and `objdump`.
It prints the zig it used, the binary's path, its SHA-256 and the highest GLIBC symbol version the binary needs, and fails when that version is above 2.34 or cannot be read.

## Releasing

`scripts/release.sh` builds the release assets and publishes nothing. From a clean tree whose HEAD is the tag `v<version>`
(the version is the one under `[workspace.package]` in `Cargo.toml`), it runs `scripts/build-portable.sh`, checks that the
binary's `--version` names that version and commit, and writes `dist/arca-<version>-linux-x86_64`, `dist/SHA256SUMS` and
`dist/RELEASE-NOTES.md` (that version's section of `CHANGELOG.md`). It then prints the `gh release create` command for
those files and the glibc the binary needs; it does not run it. `scripts/release.sh --rehearse` builds and checks the
same files without the tag, to try the script before the tag exists, and prints no publish command.
