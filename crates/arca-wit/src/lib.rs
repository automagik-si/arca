//! The plugin contract as a WIT world, and the check that reads its imports and exports from the parsed world.
//!
//! The world declares what a plugin may use. It imports one interface, the kernel's `host`, and exports one, the plugin's
//! `guest`. No `wasi:*` package is imported, so a plugin has no clock, no filesystem and no network of its own.

/// The WIT source of `arca:plugin@0.1.0`.
pub const WORLD: &str = include_str!("../../../wit/arca-plugin.wit");

/// The package the world belongs to.
pub const PACKAGE: &str = "arca:plugin@0.1.0";

/// The name of the world inside the package.
const WORLD_NAME: &str = "plugin";

/// Parses `sources` in order, selects `world` from the package pushed last, and returns the world's imports.
///
/// An interface import is named by its package-qualified name, such as `arca:plugin/host@0.1.0`. A function or type
/// import is named by its bare name, so that no import can go unlisted. The errors are the parser's messages.
pub fn imports_of(sources: &[(&str, &str)], world: &str) -> Result<Vec<String>, String> {
    let mut resolve = wit_parser::Resolve::default();
    let mut last = None;
    for (path, text) in sources {
        let id = resolve
            .push_str(path, text)
            .map_err(|err| format!("{err:?}"))?;
        last = Some(id);
    }
    let last = last.ok_or_else(|| "no WIT source was given".to_string())?;
    let world_id = resolve
        .select_world(&[last], Some(world))
        .map_err(|err| format!("{err:?}"))?;

    let mut names = Vec::new();
    for key in resolve.worlds[world_id].imports.values() {
        let name = match key {
            wit_parser::WorldItem::Interface { id, .. } => resolve
                .id_of(*id)
                .ok_or_else(|| "an imported interface has no name".to_string())?,
            wit_parser::WorldItem::Function(function) => function.name.clone(),
            wit_parser::WorldItem::Type { id, .. } => resolve.types[*id]
                .name
                .clone()
                .ok_or_else(|| "an imported type has no name".to_string())?,
        };
        names.push(name);
    }
    Ok(names)
}

/// The imports of the `plugin` world in [`WORLD`].
pub fn world_imports() -> Result<Vec<String>, String> {
    imports_of(&[("arca-plugin.wit", WORLD)], WORLD_NAME)
}
