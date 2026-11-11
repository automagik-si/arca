//! The `write` feature of this crate is off by default, and only `arca-providers` may enable it. Two checks read the manifests
//! as text: the workspace carries no TOML parser, and they add no dependency. A third reads the resolved graph from
//! `cargo metadata`, which normalizes renamed dependencies and every feature syntax, and names each package that enables the
//! feature.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value, json};

/// The crate whose write feature is under test.
const INCUS: &str = "arca-incus";
/// The feature that turns the writer on.
const WRITE: &str = "write";

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
    // A dependency for one platform only is not in another platform's resolved graph, so this check has to see it.
    assert!(enables_write(
        "[target.'cfg(windows)'.dependencies]\narca-incus = { workspace = true, features = [\"write\"] }\n"
    ));
    assert!(enables_write(
        "[target.'cfg(windows)'.dependencies.arca-incus]\nworkspace = true\nfeatures = [\"write\"]\n"
    ));
    assert!(!enables_write(
        "[dependencies]\narca-incus.workspace = true\nserde.workspace = true\n"
    ));
}

/// The platform this test runs on, as cargo names it.
fn host(cargo: &std::ffi::OsStr) -> String {
    let output = Command::new(cargo)
        .arg("-vV")
        .output()
        .expect("cargo prints its version");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("cargo names its host")
        .trim()
        .to_string()
}

/// The workspace's resolved graph, as `cargo metadata` prints it. Each dependency keeps its real package name, and every
/// package's features are already resolved, so no manifest is parsed here.
///
/// It is the graph of the platform the test runs on. `cargo metadata` reads the manifest of every package it lists, and
/// without the filter it lists the packages of every platform the lock file names. A machine that has only built the
/// workspace holds the crates of its own platform, so there the unfiltered call has to download the others and, offline,
/// fails. The filter leaves out one thing: a dependency on `arca-incus` declared for another platform only. The manifest
/// check above reads those tables too, and `the_detector_flags_every_way_of_enabling_the_feature` holds it to that.
fn resolved_graph() -> Value {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let manifest = crates_dir().join("..").join("Cargo.toml");
    let output = Command::new(&cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--offline",
            "--filter-platform",
        ])
        .arg(host(&cargo))
        .arg("--manifest-path")
        .arg(&manifest)
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata prints JSON")
}

fn array(value: &Value) -> &[Value] {
    value.as_array().expect("a JSON array")
}

fn text(value: &Value) -> &str {
    value.as_str().expect("a JSON string")
}

/// The strings of an array, and none for a value that is not one.
fn strings(value: &Value) -> Vec<String> {
    value.as_array().map_or_else(Vec::new, |items| {
        items.iter().map(|item| text(item).to_string()).collect()
    })
}

/// Each package of the metadata, by its id.
fn packages_by_id(metadata: &Value) -> BTreeMap<&str, &Value> {
    array(&metadata["packages"])
        .iter()
        .map(|package| (text(&package["id"]), package))
        .collect()
}

/// The package name of a resolved id.
fn name_of(packages: &BTreeMap<&str, &Value>, id: &str) -> String {
    text(&packages.get(id).expect("a package the resolve names")["name"]).to_string()
}

/// The features the resolve enables on the package called `name`. The package must be in the resolve.
fn resolved_features(metadata: &Value, name: &str) -> Vec<String> {
    let packages = packages_by_id(metadata);
    array(&metadata["resolve"]["nodes"])
        .iter()
        .find(|node| name_of(&packages, text(&node["id"])) == name)
        .map(|node| strings(&node["features"]))
        .unwrap_or_else(|| panic!("the resolved graph holds {name}"))
}

/// The packages whose enabled features give `arca-incus` its `write` feature. A package counts when it depends on
/// `arca-incus` in the resolve and either lists `write` in its dependency entry, or reaches `arca-incus/write` through its
/// own feature table. A dependency that is not active is not in the resolve, so it counts for nothing.
fn write_enablers(metadata: &Value) -> BTreeSet<String> {
    let packages = packages_by_id(metadata);
    let mut enablers = BTreeSet::new();
    for node in array(&metadata["resolve"]["nodes"]) {
        let package = packages.get(text(&node["id"])).expect("a resolved package");
        let depends = array(&node["deps"])
            .iter()
            .any(|dep| name_of(&packages, text(&dep["pkg"])) == INCUS);
        if !depends {
            continue;
        }
        let listed = array(&package["dependencies"]).iter().any(|dep| {
            text(&dep["name"]) == INCUS
                && strings(&dep["features"])
                    .iter()
                    .any(|f| f.as_str() == WRITE)
        });
        let table = package["features"].as_object().expect("a feature table");
        if listed || reaches_write(table, &strings(&node["features"])) {
            enablers.insert(text(&package["name"]).to_string());
        }
    }
    enablers
}

/// Whether the enabled features reach `arca-incus/write`, or the weak `arca-incus?/write`, through the package's own feature
/// table. A local feature is followed. A `dep:` entry only activates a dependency, so it leads nowhere here.
fn reaches_write(table: &Map<String, Value>, enabled: &[String]) -> bool {
    let mut pending: Vec<String> = enabled.to_vec();
    let mut seen = BTreeSet::new();
    while let Some(feature) = pending.pop() {
        if !seen.insert(feature.clone()) {
            continue;
        }
        let Some(entries) = table.get(&feature) else {
            continue;
        };
        for entry in strings(entries) {
            match entry.split_once('/') {
                Some((dep, name)) => {
                    let dep = dep
                        .strip_prefix("dep:")
                        .unwrap_or(dep)
                        .trim_end_matches('?');
                    if dep == INCUS && name == WRITE {
                        return true;
                    }
                }
                None => pending.push(entry),
            }
        }
    }
    false
}

#[test]
fn the_resolved_graph_gives_the_write_feature_to_arca_providers_only() {
    let metadata = resolved_graph();
    let enablers = write_enablers(&metadata);
    let outside: Vec<&String> = enablers
        .iter()
        .filter(|name| name.as_str() != PROVIDER)
        .collect();
    assert!(
        outside.is_empty(),
        "packages outside {PROVIDER} enable arca-incus's write feature: {outside:?}"
    );
    // The resolve says whether anything enables the feature, so a detector that finds no enabler must agree with it.
    let resolved = resolved_features(&metadata, INCUS);
    if resolved.iter().any(|feature| feature == WRITE) {
        assert!(
            !enablers.is_empty(),
            "the resolved graph enables write on arca-incus, and no dependent explains it"
        );
    }
}

#[test]
fn the_resolved_graph_check_flags_renamed_dependencies_and_every_feature_syntax() {
    // Three enablers, each by a different route: a renamed dependency that lists `write`, a feature chain that reaches
    // `arca-incus/write`, and a weak `arca-incus?/write`. Four packages must not count: one whose `dep:` entry only
    // activates the dependency, one whose optional dependency is not active, one whose feature is another crate's write,
    // and the writer itself.
    let metadata = json!({
        "packages": [
            {"id": "p-incus", "name": "arca-incus", "features": {"write": []}, "dependencies": []},
            {"id": "p-kernel", "name": "arca-kernel", "features": {}, "dependencies": []},
            {"id": "p-renamed", "name": "renamed", "features": {},
                "dependencies": [{"name": "arca-incus", "rename": "incus", "features": ["write"], "optional": false, "kind": null}]},
            {"id": "p-chain", "name": "chain",
                "features": {"all": ["incus-write"], "incus-write": ["dep:arca-incus", "arca-incus/write"]},
                "dependencies": [{"name": "arca-incus", "rename": null, "features": [], "optional": true, "kind": null}]},
            {"id": "p-weak", "name": "weak", "features": {"maybe": ["arca-incus?/write"]},
                "dependencies": [{"name": "arca-incus", "rename": null, "features": [], "optional": true, "kind": null}]},
            {"id": "p-plain", "name": "plain", "features": {"incus": ["dep:arca-incus"]},
                "dependencies": [{"name": "arca-incus", "rename": null, "features": [], "optional": true, "kind": null}]},
            {"id": "p-inactive", "name": "inactive", "features": {},
                "dependencies": [{"name": "arca-incus", "rename": null, "features": ["write"], "optional": true, "kind": null}]},
            {"id": "p-other", "name": "other", "features": {"write": ["arca-kernel/write"]},
                "dependencies": [{"name": "arca-kernel", "rename": null, "features": ["write"], "optional": false, "kind": null}]}
        ],
        "resolve": {"nodes": [
            {"id": "p-incus", "deps": [], "features": ["write"]},
            {"id": "p-kernel", "deps": [], "features": []},
            {"id": "p-renamed", "deps": [{"name": "incus", "pkg": "p-incus"}], "features": []},
            {"id": "p-chain", "deps": [{"name": "arca-incus", "pkg": "p-incus"}], "features": ["all"]},
            {"id": "p-weak", "deps": [{"name": "arca-incus", "pkg": "p-incus"}], "features": ["maybe"]},
            {"id": "p-plain", "deps": [{"name": "arca-incus", "pkg": "p-incus"}], "features": ["incus"]},
            {"id": "p-inactive", "deps": [], "features": []},
            {"id": "p-other", "deps": [{"name": "arca-kernel", "pkg": "p-kernel"}], "features": ["write"]}
        ]}
    });
    let expected: BTreeSet<String> = ["chain", "renamed", "weak"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(write_enablers(&metadata), expected);
}
