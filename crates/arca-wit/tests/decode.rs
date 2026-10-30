//! A component built against `arca:plugin@0.1.0` imports only the kernel's host interface. The component is built the way a
//! plugin is built: a dummy core module with the world's imports and exports, the component metadata embedded, and the
//! component encoded. The check then reads the world back through the decoder, so it does not trust the text it started from.

use arca_wit::{PACKAGE, WASI_CLOCKS_STUB, WORLD};
use wit_component::{
    ComponentEncoder, DecodedWasm, StringEncoding, decode, dummy_module, embed_component_metadata,
};
use wit_parser::{ManglingAndAbi, Resolve, WorldId, WorldItem};

/// Parses each `(path, text)` source in order and selects the `plugin` world of the last one.
fn plugin_world(sources: &[(&str, &str)]) -> (Resolve, WorldId) {
    let mut resolve = Resolve::default();
    let mut last = None;
    for &(path, text) in sources {
        last = Some(
            resolve
                .push_str(path, text)
                .expect("the source parses as WIT"),
        );
    }
    let package = last.expect("at least one WIT source is given");
    let world = resolve
        .select_world(&[package], Some("plugin"))
        .expect("the world `plugin` exists");
    (resolve, world)
}

/// Builds a component for `world` of `resolve`: a dummy core module, its embedded component metadata, and the encoding.
fn build_component(resolve: &Resolve, world: WorldId) -> Vec<u8> {
    let mut module = dummy_module(resolve, world, ManglingAndAbi::Standard32);
    // `false` matches `ComponentEncoder::default()`, which keeps the full package versions in the decoded names.
    embed_component_metadata(&mut module, resolve, world, StringEncoding::UTF8, false)
        .expect("the component metadata embeds");
    let mut encoder = ComponentEncoder::default();
    encoder
        .module(&module)
        .expect("the module is accepted")
        .validate(true)
        .encode()
        .expect("the component encodes")
}

/// Decodes a component into its resolver and its world.
fn decode_component(bytes: &[u8]) -> (Resolve, WorldId) {
    match decode(bytes).expect("the component decodes") {
        DecodedWasm::Component(resolve, world) => (resolve, world),
        DecodedWasm::WitPackage(..) => panic!("the component decoded as a WIT package"),
    }
}

/// Each import of `world` as its package-qualified name, with the namespace of the package it belongs to.
fn imports_of(resolve: &Resolve, world: WorldId) -> Vec<(String, String)> {
    resolve.worlds[world]
        .imports
        .values()
        .map(|item| match item {
            WorldItem::Interface { id, .. } => {
                let name = resolve
                    .id_of(*id)
                    .expect("an imported interface has a name");
                let package = resolve.interfaces[*id]
                    .package
                    .expect("an imported interface belongs to a package");
                (name, resolve.packages[package].name.namespace.clone())
            }
            other => panic!("an import that is not an interface: {other:?}"),
        })
        .collect()
}

/// The names of the imports of `world` that belong to the `wasi` namespace.
fn wasi_imports(resolve: &Resolve, world: WorldId) -> Vec<String> {
    imports_of(resolve, world)
        .into_iter()
        .filter(|(_, namespace)| namespace == "wasi")
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn the_decoded_world_imports_only_the_host_interface() {
    let (resolve, world) = plugin_world(&[("arca-plugin.wit", WORLD)]);
    let component = build_component(&resolve, world);
    let (decoded, decoded_world) = decode_component(&component);

    let imports = imports_of(&decoded, decoded_world);
    assert_eq!(
        imports,
        vec![("arca:plugin/host@0.1.0".to_string(), "arca".to_string())]
    );
    assert!(
        wasi_imports(&decoded, decoded_world).is_empty(),
        "a wasi import: {imports:?}"
    );

    // The decoder wraps the component in its own root package, so the contract's version is read from the imported
    // interface's package, which must still be `arca:plugin@0.1.0`.
    let host = match decoded.worlds[decoded_world].imports.values().next() {
        Some(WorldItem::Interface { id, .. }) => *id,
        other => panic!("the only import is the host interface: {other:?}"),
    };
    let package = decoded.interfaces[host]
        .package
        .expect("the host interface belongs to a package");
    assert_eq!(decoded.packages[package].name.to_string(), PACKAGE);
}

#[test]
fn a_wasi_clock_import_in_the_world_is_detected() {
    // The probe is a copy of the world with one wasi import added. The stub package is pushed first, so the import
    // resolves, and the probe keeps the `arca:plugin` package name.
    let probe = WORLD.replace(
        "world plugin {",
        "world plugin {\n  import wasi:clocks/wall-clock@0.2.0;",
    );
    assert_ne!(probe, WORLD, "the probe adds the wasi import");
    let (resolve, world) = plugin_world(&[
        ("wasi-clocks.wit", WASI_CLOCKS_STUB),
        ("arca-plugin-probe.wit", probe.as_str()),
    ]);
    let component = build_component(&resolve, world);
    let (decoded, decoded_world) = decode_component(&component);

    assert_eq!(
        wasi_imports(&decoded, decoded_world),
        vec!["wasi:clocks/wall-clock@0.2.0".to_string()]
    );
    let imports = imports_of(&decoded, decoded_world);
    assert!(
        imports
            .iter()
            .any(|(name, _)| name == "arca:plugin/host@0.1.0"),
        "imports: {imports:?}"
    );
}
