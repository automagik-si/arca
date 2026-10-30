//! The plugin boundary (P1). A plugin crate is one whose `[package.metadata.arca]` table sets `kind = "plugin"`. Such a crate
//! may not name the Incus client, the providers or the approval verifier under `[dependencies]`, and it may not carry a build
//! table or a target table at all. `[dev-dependencies]` may name them, so tests can use the fakes. A kernel-side crate is not
//! bound by this test.

use std::fs;
use std::path::{Path, PathBuf};

/// The crates a plugin may never name in its normal dependencies.
const FORBIDDEN: [&str; 3] = ["arca-incus", "arca-providers", "arca-approval"];

/// The only crates that may declare `kind = "plugin"` (Decision 3).
const PLUGIN_CRATES: [&str; 4] = ["arca-watch", "arca-netproject", "arca-postgres", "arca-vm"];

/// What a manifest declares, read line by line. Only the shapes this workspace uses are read: table headers, `key = value`
/// lines, and the dotted or inline forms of a dependency.
#[derive(Debug, Default, PartialEq, Eq)]
struct Declared {
    plugin: bool,
    headers: Vec<String>,
    /// `(table, dependency)` for each dependency a `[dependencies]` table or its `[dependencies.<name>]` form names.
    deps: Vec<(String, String)>,
    /// The `package = "..."` renames a dependency under those tables carries.
    renames: Vec<String>,
}

fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (index, c) in line.char_indices() {
        match (quote, c) {
            (None, '#') => return &line[..index],
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), close) if open == close => quote = None,
            _ => {}
        }
    }
    line
}

fn unquote(text: &str) -> &str {
    let text = text.trim();
    text.strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            text.strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(text)
}

/// The `package` value of an inline dependency table such as `{ package = "arca-incus", workspace = true }`.
fn package_of(value: &str) -> Option<String> {
    let index = value.find("package")?;
    let rest = value[index + "package".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn parse(text: &str) -> Declared {
    let mut declared = Declared::default();
    // The table the current line sits under, and whether it is a dependency table whose keys we read.
    let mut table = String::new();
    let mut dep_table: Option<String> = None;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(inner) = line
            .strip_prefix("[[")
            .and_then(|rest| rest.split("]]").next())
        {
            table = inner.trim().to_string();
            declared.headers.push(table.clone());
            dep_table = None;
            continue;
        }
        if let Some(inner) = line
            .strip_prefix('[')
            .and_then(|rest| rest.split(']').next())
        {
            table = inner.trim().to_string();
            declared.headers.push(table.clone());
            dep_table = None;
            if let Some(name) = table.strip_prefix("dependencies.") {
                declared
                    .deps
                    .push(("dependencies".to_string(), unquote(name).to_string()));
                dep_table = Some(table.clone());
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if table == "package.metadata.arca" && unquote(key) == "kind" {
            declared.plugin = unquote(value) == "plugin";
            continue;
        }
        let in_deps = table == "dependencies" || dep_table.as_deref() == Some(table.as_str());
        if !in_deps {
            continue;
        }
        if dep_table.is_some() {
            // Keys inside `[dependencies.<name>]` describe that one dependency.
            if unquote(key) == "package" {
                declared.renames.push(unquote(value).to_string());
            }
            continue;
        }
        let name = unquote(key.split('.').next().unwrap_or(key));
        declared
            .deps
            .push(("dependencies".to_string(), name.to_string()));
        if let Some(renamed) = package_of(value) {
            declared.renames.push(renamed);
        }
    }
    declared
}

/// Every way a plugin manifest breaks the boundary. A manifest that is not a plugin has none. A violation names a table or a
/// dependency, never a value.
fn violations(text: &str) -> Vec<String> {
    let declared = parse(text);
    if !declared.plugin {
        return Vec::new();
    }
    let mut found = Vec::new();
    for header in &declared.headers {
        let outright = header == "build-dependencies"
            || header.starts_with("build-dependencies.")
            || (header.starts_with("target.")
                && (header.contains(".dependencies") || header.contains(".build-dependencies")));
        if outright {
            found.push(format!("the plugin has the table [{header}]"));
        }
    }
    for (table, name) in &declared.deps {
        if FORBIDDEN.contains(&name.as_str()) {
            found.push(format!("[{table}] names {name}"));
        }
    }
    for renamed in &declared.renames {
        if FORBIDDEN.contains(&renamed.as_str()) {
            found.push("a dependency is renamed to a forbidden crate".to_string());
        }
    }
    found
}

fn is_plugin(text: &str) -> bool {
    parse(text).plugin
}

/// The `crates/` directory of this workspace.
fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("the crates directory exists")
}

fn each_manifest() -> Vec<(String, String)> {
    let mut found = Vec::new();
    for entry in fs::read_dir(crates_dir()).expect("read the crates directory") {
        let dir = entry.expect("a crate directory entry").path();
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let name = dir
            .file_name()
            .expect("a crate directory has a name")
            .to_string_lossy()
            .into_owned();
        found.push((
            name,
            fs::read_to_string(&manifest).expect("read a Cargo.toml"),
        ));
    }
    found.sort();
    found
}

const PLUGIN_NAMING_INCUS: &str = r#"
[package]
name = "fixture"

[package.metadata.arca]
kind = "plugin"

[dependencies]
arca-incus.workspace = true
arca-kernel.workspace = true
"#;

const PLUGIN_NAMING_INCUS_IN_DEV: &str = r#"
[package]
name = "fixture"

[package.metadata.arca]
kind = "plugin"

[dependencies]
arca-kernel.workspace = true

[dev-dependencies]
arca-incus.workspace = true
arca-providers.workspace = true
"#;

#[test]
fn a_plugin_naming_the_incus_client_under_dependencies_fails() {
    let found = violations(PLUGIN_NAMING_INCUS);
    assert!(
        found.iter().any(|v| v.contains("arca-incus")),
        "the boundary names arca-incus: {found:?}"
    );
}

#[test]
fn a_plugin_naming_the_incus_client_only_under_dev_dependencies_passes() {
    assert_eq!(violations(PLUGIN_NAMING_INCUS_IN_DEV), Vec::<String>::new());
}

#[test]
fn a_plugin_naming_each_forbidden_crate_under_dependencies_fails() {
    for name in FORBIDDEN {
        let manifest = format!(
            "[package.metadata.arca]\nkind = \"plugin\"\n[dependencies]\n{name} = {{ workspace = true }}\n"
        );
        assert!(!violations(&manifest).is_empty(), "{name} is refused");
    }
}

#[test]
fn a_plugin_naming_a_forbidden_crate_in_the_dotted_table_form_fails() {
    let manifest = "[package.metadata.arca]\nkind = \"plugin\"\n[dependencies.arca-providers]\nworkspace = true\n";
    assert!(!violations(manifest).is_empty());
}

#[test]
fn a_plugin_renaming_a_dependency_to_a_forbidden_crate_fails() {
    let inline = "[package.metadata.arca]\nkind = \"plugin\"\n[dependencies]\nsneaky = { package = \"arca-incus\", workspace = true }\n";
    assert!(
        !violations(inline).is_empty(),
        "the inline rename is refused"
    );
    let table = "[package.metadata.arca]\nkind = \"plugin\"\n[dependencies.sneaky]\npackage = \"arca-approval\"\nworkspace = true\n";
    assert!(!violations(table).is_empty(), "the table rename is refused");
}

#[test]
fn a_plugin_with_a_build_dependencies_table_fails_even_when_it_is_empty() {
    let manifest = "[package.metadata.arca]\nkind = \"plugin\"\n[build-dependencies]\n";
    assert!(!violations(manifest).is_empty());
}

#[test]
fn a_plugin_with_a_target_dependencies_table_fails_outright() {
    let manifest =
        "[package.metadata.arca]\nkind = \"plugin\"\n[target.'cfg(unix)'.dependencies]\n";
    assert!(!violations(manifest).is_empty());
    let dotted = "[package.metadata.arca]\nkind = \"plugin\"\n[target.x86_64-unknown-linux-gnu.dependencies.arca-incus]\nworkspace = true\n";
    assert!(!violations(dotted).is_empty());
}

#[test]
fn a_kernel_side_crate_may_name_the_incus_client() {
    let manifest =
        "[package]\nname = \"arca-metrics\"\n[dependencies]\narca-incus.workspace = true\n";
    assert_eq!(violations(manifest), Vec::<String>::new());
    assert!(!is_plugin(manifest));
}

#[test]
fn a_comment_that_names_a_forbidden_crate_is_not_a_dependency() {
    let manifest = "[package.metadata.arca]\nkind = \"plugin\"\n[dependencies]\narca-kernel.workspace = true # arca-incus is not used\n";
    assert_eq!(violations(manifest), Vec::<String>::new());
}

#[test]
fn every_crate_in_the_workspace_honours_the_boundary() {
    let mut plugins = Vec::new();
    for (name, text) in each_manifest() {
        let found = violations(&text);
        assert!(
            found.is_empty(),
            "{name} breaks the plugin boundary: {found:?}"
        );
        if is_plugin(&text) {
            plugins.push(name);
        }
    }
    for plugin in &plugins {
        assert!(
            PLUGIN_CRATES.contains(&plugin.as_str()),
            "{plugin} declares kind = \"plugin\" but is not one of the plugin crates"
        );
    }
    assert!(
        plugins.iter().any(|p| p == "arca-watch"),
        "arca-watch is a plugin"
    );
}

#[test]
fn arca_metrics_is_not_a_plugin_crate() {
    let (_, text) = each_manifest()
        .into_iter()
        .find(|(name, _)| name == "arca-metrics")
        .expect("arca-metrics is a workspace crate");
    assert!(
        !is_plugin(&text),
        "arca-metrics is a kernel-side sampler (Decision 3)"
    );
}
