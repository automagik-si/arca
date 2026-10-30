//! The WIT world, read through the parser. The checks run on the parsed world, not on the text, so a typo in the file fails
//! here and not only at a later host call.

use arca_wit::{PACKAGE, WASI_CLOCKS_STUB, WORLD, imports_of, world_imports};

/// Parses the world and returns the resolver, the package it belongs to, and the world's id.
fn parse_world() -> (
    wit_parser::Resolve,
    wit_parser::PackageId,
    wit_parser::WorldId,
) {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve
        .push_str("arca-plugin.wit", WORLD)
        .expect("the world parses");
    let world = resolve
        .select_world(&[package], Some("plugin"))
        .expect("the world `plugin` exists");
    (resolve, package, world)
}

/// The names of the functions an interface of the parsed package declares.
fn functions_of(resolve: &wit_parser::Resolve, name: &str) -> Vec<String> {
    let (_, interface) = resolve
        .interfaces
        .iter()
        .find(|(_, interface)| interface.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("interface `{name}` is declared"));
    interface.functions.keys().cloned().collect()
}

#[test]
fn package_name_and_version_are_arca_plugin_0_1_0() {
    let (resolve, package, _world) = parse_world();
    assert_eq!(resolve.packages[package].name.to_string(), PACKAGE);
    assert_eq!(PACKAGE, "arca:plugin@0.1.0");
}

#[test]
fn world_imports_exactly_the_host_interface() {
    // The types that cross the boundary live in `host`, so the world has one import. Any second import, a type package
    // included, would be a second grant and fails this exact match.
    let imports = world_imports().expect("the world parses");
    assert_eq!(imports, vec!["arca:plugin/host@0.1.0".to_string()]);
}

#[test]
fn world_exports_exactly_the_guest_interface() {
    let (resolve, _package, world) = parse_world();
    let exports = &resolve.worlds[world].exports;
    assert_eq!(
        exports.len(),
        1,
        "exports: {:?}",
        exports.keys().collect::<Vec<_>>()
    );
    let names: Vec<String> = exports
        .values()
        .map(|item| match item {
            wit_parser::WorldItem::Interface { id, .. } => resolve
                .id_of(*id)
                .expect("the exported interface has a name"),
            other => panic!("an export that is not an interface: {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["arca:plugin/guest@0.1.0".to_string()]);
}

#[test]
fn host_and_guest_declare_the_contract_functions() {
    let (resolve, _package, _world) = parse_world();
    assert_eq!(
        functions_of(&resolve, "host"),
        ["effect", "observe", "provide"]
    );
    assert_eq!(functions_of(&resolve, "guest"), ["manifest", "apply"]);
}

#[test]
fn no_import_is_in_the_wasi_namespace() {
    let imports = world_imports().expect("the world parses");
    assert!(!imports.is_empty(), "the world imports the host");
    for import in &imports {
        assert!(!import.starts_with("wasi:"), "a wasi import: {import}");
    }
}

#[test]
fn a_wasi_import_added_to_the_world_is_reported() {
    // The probe is a copy of the world with one wasi import added. It keeps the `arca:plugin` package name, because only
    // one copy of that package is pushed into this resolver.
    let probe = WORLD.replace(
        "world plugin {",
        "world plugin {\n  import wasi:clocks/wall-clock@0.2.0;",
    );
    assert_ne!(probe, WORLD, "the probe adds the wasi import");
    let imports = imports_of(
        &[
            ("wasi-clocks.wit", WASI_CLOCKS_STUB),
            ("arca-plugin-probe.wit", &probe),
        ],
        "plugin",
    )
    .expect("the probe parses");
    assert!(
        imports.contains(&"wasi:clocks/wall-clock@0.2.0".to_string()),
        "imports: {imports:?}"
    );
    assert!(
        imports.contains(&"arca:plugin/host@0.1.0".to_string()),
        "imports: {imports:?}"
    );
}

#[test]
fn an_empty_source_list_is_refused() {
    let error = imports_of(&[], "plugin").expect_err("no source");
    assert!(error.contains("no WIT source"), "error: {error}");
}

#[test]
fn an_unknown_world_is_refused() {
    let error = imports_of(&[("arca-plugin.wit", WORLD)], "missing").expect_err("no such world");
    assert!(!error.is_empty());
}

#[test]
fn malformed_wit_is_refused() {
    let error = imports_of(
        &[("broken.wit", "package arca:plugin@0.1.0; world plugin {")],
        "plugin",
    )
    .expect_err("the source does not parse");
    assert!(!error.is_empty());
}
