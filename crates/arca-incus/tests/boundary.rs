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
    // Two spellings of a dependency for one platform only. This text check sees these two and not every spelling cargo
    // accepts; the graph check below reads what cargo itself made of the manifests, for every platform.
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
    assert!(
        output.status.success(),
        "cargo -vV failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("cargo names its host")
        .trim()
        .to_string()
}

/// The resolved graph of the workspace at `manifest`, as `cargo metadata` prints it. Each dependency keeps its real package
/// name, and every package's features are already resolved, so no manifest is parsed here.
///
/// The resolve is the one of the platform the test runs on. `cargo metadata` reads the manifest of every package it lists,
/// and without the filter it lists the packages of every platform the lock file names. A machine that has only built the
/// workspace holds the crates of its own platform, so there the unfiltered call has to download the others and, offline,
/// fails. The filter narrows the resolve and the list of packages. It does not narrow what a package declares: each
/// package's `dependencies` still carries its entries for every platform, each with its `target`. [`write_enablers`] reads
/// those, so a dependency declared for another platform only is still seen.
fn resolved_graph(manifest: &Path) -> Value {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
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
        .arg(manifest)
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

/// The packages that give `arca-incus` its `write` feature. Every package is judged by what it declares, for every
/// platform, and not by whether this platform's resolve links it to `arca-incus`: a dependency declared for another
/// platform only is in no resolve this test can ask for offline. A package counts when
///
/// - an entry for `arca-incus` among its declared dependencies lists `write`, and that entry is in use; or
/// - its enabled features reach `<key>/write`, or the weak `<key>?/write` while the dependency is in use.
///
/// `<key>` is the name the package gives the dependency: `arca-incus`, or the new name of a renamed one. A dependency is
/// in use when it is not optional, when the resolve links the package to `arca-incus`, or when an enabled feature turns it
/// on. An optional dependency that nothing turns on counts for nothing.
fn write_enablers(metadata: &Value) -> BTreeSet<String> {
    let packages = packages_by_id(metadata);
    let nodes: BTreeMap<&str, &Value> = array(&metadata["resolve"]["nodes"])
        .iter()
        .map(|node| (text(&node["id"]), node))
        .collect();
    let mut enablers = BTreeSet::new();
    for (id, package) in &packages {
        let entries: Vec<&Value> = array(&package["dependencies"])
            .iter()
            .filter(|dep| text(&dep["name"]) == INCUS)
            .collect();
        if entries.is_empty() {
            continue;
        }
        let node = nodes.get(id);
        let enabled = node.map_or_else(Vec::new, |node| strings(&node["features"]));
        let linked = node.is_some_and(|node| {
            array(&node["deps"])
                .iter()
                .any(|dep| name_of(&packages, text(&dep["pkg"])) == INCUS)
        });
        let table = package["features"].as_object().expect("a feature table");
        let keys: BTreeSet<&str> = entries.iter().map(|dep| key_of(dep)).collect();
        let reached = reach(table, &enabled, &keys);
        let in_use = |dep: &Value| {
            dep["optional"].as_bool() != Some(true) || linked || reached.on.contains(key_of(dep))
        };
        let listed = entries
            .iter()
            .any(|dep| strings(&dep["features"]).iter().any(|f| f == WRITE) && in_use(dep));
        let weak = entries
            .iter()
            .any(|dep| reached.weak.contains(key_of(dep)) && in_use(dep));
        if listed || reached.write || weak {
            enablers.insert(text(&package["name"]).to_string());
        }
    }
    enablers
}

/// The name a package gives a dependency: its new name when it is renamed, and the package's name otherwise.
fn key_of(dep: &Value) -> &str {
    dep["rename"].as_str().unwrap_or_else(|| text(&dep["name"]))
}

/// What a package's enabled features do to its dependencies on `arca-incus`.
#[derive(Default)]
struct Reach<'a> {
    /// A feature names `<key>/write`: the dependency is turned on, with its writer.
    write: bool,
    /// The keys a feature names as `<key>?/write`: the writer, but only where the dependency is in use.
    weak: BTreeSet<&'a str>,
    /// The keys a feature turns on, as `dep:<key>` or as `<key>/<feature>`.
    on: BTreeSet<&'a str>,
}

/// Follows the enabled features through the package's own feature table. `keys` are the names the package gives its
/// dependencies on `arca-incus`. A local feature is followed; an entry for any other dependency leads nowhere here.
fn reach<'a>(
    table: &Map<String, Value>,
    enabled: &[String],
    keys: &BTreeSet<&'a str>,
) -> Reach<'a> {
    let mut found = Reach::default();
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
            if let Some(dep) = entry.strip_prefix("dep:") {
                if let Some(key) = keys.get(dep) {
                    found.on.insert(*key);
                }
                continue;
            }
            let Some((dep, name)) = entry.split_once('/') else {
                pending.push(entry);
                continue;
            };
            let Some(key) = keys.get(dep.trim_end_matches('?')) else {
                continue;
            };
            if dep.ends_with('?') {
                if name == WRITE {
                    found.weak.insert(*key);
                }
            } else {
                found.on.insert(*key);
                found.write |= name == WRITE;
            }
        }
    }
    found
}

#[test]
fn the_resolved_graph_gives_the_write_feature_to_arca_providers_only() {
    let metadata = resolved_graph(&crates_dir().join("..").join("Cargo.toml"));
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
    // Five enablers, each by a different route: a renamed dependency that lists `write`, a feature chain that reaches
    // `arca-incus/write`, a weak `arca-incus?/write`, a dependency declared for another platform only, which no resolve
    // of this platform links, and a feature that reaches the writer through a renamed dependency's new name. Five
    // packages must not count: one whose `dep:` entry only activates the dependency, one whose optional dependency is not
    // active, the same for another platform only, one whose feature is another crate's write, and the writer itself.
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
                "dependencies": [{"name": "arca-kernel", "rename": null, "features": ["write"], "optional": false, "kind": null}]},
            {"id": "p-macos", "name": "macos-only", "features": {},
                "dependencies": [{"name": "arca-incus", "rename": null, "features": ["write"], "optional": false, "kind": null,
                    "target": "cfg(target_os = \"macos\")"}]},
            {"id": "p-macos-off", "name": "macos-optional", "features": {},
                "dependencies": [{"name": "arca-incus", "rename": null, "features": ["write"], "optional": true, "kind": null,
                    "target": "cfg(target_os = \"macos\")"}]},
            {"id": "p-alias", "name": "alias", "features": {"w": ["incus/write"]},
                "dependencies": [{"name": "arca-incus", "rename": "incus", "features": [], "optional": false, "kind": null}]}
        ],
        "resolve": {"nodes": [
            {"id": "p-incus", "deps": [], "features": ["write"]},
            {"id": "p-kernel", "deps": [], "features": []},
            {"id": "p-renamed", "deps": [{"name": "incus", "pkg": "p-incus"}], "features": []},
            {"id": "p-chain", "deps": [{"name": "arca-incus", "pkg": "p-incus"}], "features": ["all"]},
            {"id": "p-weak", "deps": [{"name": "arca-incus", "pkg": "p-incus"}], "features": ["maybe"]},
            {"id": "p-plain", "deps": [{"name": "arca-incus", "pkg": "p-incus"}], "features": ["incus"]},
            {"id": "p-inactive", "deps": [], "features": []},
            {"id": "p-other", "deps": [{"name": "arca-kernel", "pkg": "p-kernel"}], "features": ["write"]},
            {"id": "p-macos", "deps": [], "features": []},
            {"id": "p-macos-off", "deps": [], "features": []},
            {"id": "p-alias", "deps": [{"name": "incus", "pkg": "p-incus"}], "features": ["w"]}
        ]}
    });
    let expected: BTreeSet<String> = ["alias", "chain", "macos-only", "renamed", "weak"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(write_enablers(&metadata), expected);
}

#[test]
fn the_graph_check_names_a_member_that_enables_the_writer_for_another_platform_only() {
    // A workspace of two crates, written here and read by cargo itself. `arca-api` depends on `arca-incus` for macOS only
    // and lists `write` over several lines, as cargo allows. On any other platform no resolve links the two, and the
    // text check does not read this spelling, so only what cargo says the member declares can give it away.
    let api = "[package]\nname = \"arca-api\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
               [target.'cfg(target_os = \"macos\")'.dependencies]\n\
               arca-incus = { workspace = true, features = [\n    \"write\",\n] }\n";
    assert!(
        !enables_write(api),
        "the text check does not read this spelling"
    );
    let dir = std::env::temp_dir().join(format!("arca-boundary-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for (file, body) in [
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\n\
             [workspace.dependencies]\narca-incus = { path = \"crates/arca-incus\" }\n",
        ),
        (
            "crates/arca-incus/Cargo.toml",
            "[package]\nname = \"arca-incus\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[features]\nwrite = []\n",
        ),
        ("crates/arca-incus/src/lib.rs", ""),
        ("crates/arca-api/Cargo.toml", api),
        ("crates/arca-api/src/lib.rs", ""),
    ] {
        let path = dir.join(file);
        fs::create_dir_all(path.parent().expect("a parent directory"))
            .expect("the directory is made");
        fs::write(&path, body).expect("the file is written");
    }
    let metadata = resolved_graph(&dir.join("Cargo.toml"));
    fs::remove_dir_all(&dir).expect("the workspace is removed");
    let enablers: Vec<String> = write_enablers(&metadata).into_iter().collect();
    assert_eq!(enablers, ["arca-api"]);
}
