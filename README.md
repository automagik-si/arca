# Arca

Arca is a Rust kernel for a node's own infrastructure. The kernel reads what it owns, plans each change, and applies a
change only after the node's owner has approved it and the change is written to a journal. Each node signs its own
desired state with its own key.

This repository holds the workspace, the Apache-2.0 licence and NOTICE, and the canonical JSON that desired state and
approval statements are signed over. The kernel's other contracts come in later stages. It has no git remote until the
owner's first push.

## Build and test

The toolchain is pinned in `rust-toolchain.toml`.

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --workspace
cargo clippy --workspace -- -D warnings
```
