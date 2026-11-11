//! Records what `arca --version` prints beside the version: the short commit the binary was built from and the target
//! triple. A build with no git checkout of this workspace (a source archive, say) records `unknown` for the commit and
//! still builds.

use std::path::Path;
use std::process::Command;

/// One line of `git <args>` output, or `None` when git is missing, fails, or prints nothing.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let line = text.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// Whether the git checkout around this crate is this workspace's own: its top level is the workspace root, two
/// directories above this crate. A source archive unpacked inside some other repository is not, and git would answer
/// there with that repository's commit.
fn own_checkout() -> bool {
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    let Some(manifest_dir) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        return false;
    };
    let workspace = Path::new(&manifest_dir).join("../..");
    match (Path::new(&top).canonicalize(), workspace.canonicalize()) {
        (Ok(top), Ok(workspace)) => top == workspace,
        _ => false,
    }
}

/// Asks cargo to run this script again when `path` inside the git directory changes. A path that does not exist is
/// skipped, because cargo would otherwise run the script on every build.
fn watch(path: &str) {
    if let Some(found) = git(&["rev-parse", "--git-path", path])
        && Path::new(&found).exists()
    {
        println!("cargo:rerun-if-changed={found}");
    }
}

/// The short commit of this workspace's checkout, or `None` when there is none.
fn commit() -> Option<String> {
    if !own_checkout() {
        return None;
    }
    // HEAD moves on a checkout; the branch it names moves on a commit, as a file of its own or inside packed-refs. A
    // branch that was only in packed-refs gets its own file on the next commit, a file that did not exist when this
    // script last ran, so HEAD's log, which every commit and checkout appends to, is watched too.
    watch("HEAD");
    watch("logs/HEAD");
    watch("packed-refs");
    if let Some(branch) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
        watch(&branch);
    }
    git(&["rev-parse", "--short", "HEAD"])
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let commit = commit().unwrap_or_else(|| "unknown".to_string());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=ARCA_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=ARCA_BUILD_TARGET={target}");
}
