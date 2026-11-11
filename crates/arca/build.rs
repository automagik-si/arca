//! Records what `arca --version` prints beside the version: the short commit the binary was built from and the target
//! triple. A build with no git checkout (a source archive, say) records `unknown` for the commit and still builds.

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

/// Asks cargo to run this script again when `path` inside the git directory changes. A path that does not exist is
/// skipped, because cargo would otherwise run the script on every build.
fn watch(path: &str) {
    if let Some(found) = git(&["rev-parse", "--git-path", path])
        && Path::new(&found).exists()
    {
        println!("cargo:rerun-if-changed={found}");
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // HEAD moves on a checkout; the branch it names moves on a commit, as a file of its own or inside packed-refs.
    watch("HEAD");
    watch("packed-refs");
    if let Some(branch) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
        watch(&branch);
    }
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=ARCA_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=ARCA_BUILD_TARGET={target}");
}
