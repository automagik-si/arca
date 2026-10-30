//! The `write` feature of this crate is off by default, and only `arca-providers` may enable it. The check reads each
//! manifest as text: the workspace carries no TOML parser, and this check adds no dependency.

use std::fs;
use std::path::{Path, PathBuf};

/// The crate that is allowed to enable the writer.
const PROVIDER: &str = "arca-providers";

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// The root manifest and every member manifest, each with the name of its directory.
fn manifests() -> Vec<(String, String)> {
    let mut found = Vec::new();
    let root = crates_dir().join("..").join("Cargo.toml");
    found.push((
        "<workspace>".to_string(),
        fs::read_to_string(&root).expect("the root manifest"),
    ));
    for entry in fs::read_dir(crates_dir()).expect("the crates directory") {
        let dir = entry.expect("a crate directory entry").path();
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let name = dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .to_string();
            found.push((
                name,
                fs::read_to_string(&manifest).expect("a member manifest"),
            ));
        }
    }
    found
}

/// Whether a manifest enables the writer: a dependency on `arca-incus` that lists the `write` feature, or a feature that names
/// `arca-incus/write`. A table header ending in `.arca-incus]` opens a dependency table for it.
fn enables_write(manifest: &str) -> bool {
    let mut in_incus_table = false;
    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_incus_table = line.ends_with(".arca-incus]");
            continue;
        }
        if line.contains("arca-incus/write") {
            return true;
        }
        let names_incus = line.starts_with("arca-incus") || in_incus_table;
        if names_incus && line.contains("\"write\"") {
            return true;
        }
    }
    false
}

#[test]
fn the_write_feature_is_declared_and_off_by_default() {
    let manifest = fs::read_to_string(crates_dir().join("arca-incus").join("Cargo.toml"))
        .expect("arca-incus manifest");
    let features = manifest
        .split("[features]")
        .nth(1)
        .expect("arca-incus declares a [features] table");
    let features = features.split('[').next().unwrap_or("");
    assert!(
        features
            .lines()
            .any(|line| line.trim().starts_with("write")),
        "the write feature is declared: {features}"
    );
    assert!(
        !features
            .lines()
            .any(|line| line.trim().starts_with("default")),
        "no default feature turns the writer on: {features}"
    );
}

#[test]
fn only_arca_providers_enables_the_write_feature() {
    for (name, manifest) in manifests() {
        if name == PROVIDER {
            continue;
        }
        assert!(
            !enables_write(&manifest),
            "{name} enables arca-incus's write feature; only {PROVIDER} may"
        );
    }
}

#[test]
fn the_detector_flags_every_way_of_enabling_the_feature() {
    assert!(enables_write(
        "[dependencies]\narca-incus = { workspace = true, features = [\"write\"] }\n"
    ));
    assert!(enables_write(
        "[dependencies.arca-incus]\nworkspace = true\nfeatures = [\"write\"]\n"
    ));
    assert!(enables_write(
        "[dev-dependencies.arca-incus]\nfeatures = [\"write\"]\n"
    ));
    assert!(enables_write(
        "[features]\nincus-write = [\"arca-incus/write\"]\n"
    ));
    assert!(!enables_write(
        "[dependencies]\narca-incus.workspace = true\nserde.workspace = true\n"
    ));
}
