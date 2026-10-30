//! The systemd user units in `deploy/systemd/` and the config `docs/OPERATING.md` gives them.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

/// A file of the repository, by its path from the root.
fn repo(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

const UNITS: [&str; 2] = [
    "deploy/systemd/arca-watch.service",
    "deploy/systemd/arca-shim.service",
];

/// The values of a unit's `<key>=` lines.
fn values<'a>(unit: &'a str, key: &str) -> Vec<&'a str> {
    unit.lines()
        .filter_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        .collect()
}

/// The name of each directory the text names right under `.config/`.
fn config_dirs(text: &str) -> BTreeSet<&str> {
    text.match_indices(".config/")
        .filter_map(|(at, found)| {
            text[at + found.len()..]
                .split(['/', ' ', '`', '"', '\n'])
                .next()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

#[test]
fn no_state_directory_shares_its_name_with_a_config_directory() {
    // systemd 254 and later make a user unit's StateDirectory=<name> a link to ~/.config/<name> when that directory exists and
    // ~/.local/state/<name> does not, and serve refuses a state directory that is a link. So no unit's state directory may be
    // named as a directory the units or the operating guide keep under ~/.config.
    let operating = repo("docs/OPERATING.md");
    let units: Vec<String> = UNITS.iter().map(|path| repo(path)).collect();
    let mut config: BTreeSet<&str> = config_dirs(&operating);
    for unit in &units {
        config.extend(config_dirs(unit));
    }
    assert!(
        config.contains("arca"),
        "the guide keeps ~/.config/arca: {config:?}"
    );
    for (path, unit) in UNITS.iter().zip(&units) {
        let names = values(unit, "StateDirectory");
        assert!(!names.is_empty(), "{path} names a state directory");
        for name in names {
            assert!(
                !config.contains(name),
                "{path}: StateDirectory={name} is also a directory under ~/.config"
            );
        }
    }
}

#[test]
fn the_guides_state_dir_is_the_watch_units_state_directory() {
    let unit = repo(UNITS[0]);
    let names = values(&unit, "StateDirectory");
    assert_eq!(names.len(), 1, "{names:?}");
    let want = format!("\"stateDir\": \"<home>/.local/state/{}\"", names[0]);
    assert!(
        repo("docs/OPERATING.md").contains(&want),
        "the guide's service config names {want}"
    );
}
