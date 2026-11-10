//! `templates install --config <file> [--replace]`: puts the shipped service templates in `<stateDir>/templates`, each
//! template's JSON together with the script its entrypoint reads. The files are the repository's `templates/` as this build
//! holds them, so no checkout is needed and a template never arrives without its script.
//!
//! A file that is missing is written. A file that is already the shipped one is left. A file that differs is kept, since it
//! may be the owner's edit, unless `--replace` is given. A template the owner added under another name is never touched.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use serde_json::{Value, json};

use crate::{Failure, Flags, config};

macro_rules! shipped {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/", $name)))),*]
    };
}

/// Every file a shipped template needs on a node: each template's JSON and the script its entrypoint names. The image build
/// inputs (`automagik.build.sh`, `automagik-dsh/`) are used from a checkout and are not installed.
pub const SHIPPED: &[(&str, &str)] = shipped![
    "automagik-server.json",
    "automagik.json",
    "nats.json",
    "omni-api.json",
    "postgres-autopg.json",
    "postgres-autopg.sh",
    "workspace-vm.json",
];

pub fn install(flags: &Flags) -> Result<Option<Value>, Failure> {
    let cfg = config::load(Path::new(flags.required("config")?))?;
    install_into(&cfg.state_dir.join("templates"), flags.switch("replace")).map(Some)
}

/// Installs [`SHIPPED`] into `dir`, which is made (mode 0700) when it is missing. Answers the directory and each file's
/// name under `installed`, `unchanged` or `kept`.
pub fn install_into(dir: &Path, replace: bool) -> Result<Value, Failure> {
    let failed =
        |what: &str, error: std::io::Error| Failure::Runtime(format!("{what} ({})", error.kind()));
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(Failure::Refused(format!(
                "{} is not a directory",
                dir.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .map_err(|error| failed("the templates directory cannot be made", error))?,
        Err(error) => return Err(failed("the templates directory cannot be read", error)),
    }
    let (mut installed, mut unchanged, mut kept) = (Vec::new(), Vec::new(), Vec::new());
    for (name, text) in SHIPPED {
        let path = dir.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.is_file() => {
                return Err(Failure::Refused(format!(
                    "{} is not a regular file; it is left alone and nothing more is installed",
                    path.display()
                )));
            }
            Ok(_) => {
                let held = fs::read(&path)
                    .map_err(|error| failed("an installed template cannot be read", error))?;
                if held == text.as_bytes() {
                    unchanged.push(*name);
                    continue;
                }
                if !replace {
                    kept.push(*name);
                    continue;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(failed("an installed template cannot be read", error)),
        }
        // Written beside its place and renamed in, so a reader never sees half a file.
        let scratch = dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&scratch)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(&scratch, &path));
        if let Err(error) = written {
            let _ = fs::remove_file(&scratch);
            return Err(failed("a template cannot be written", error));
        }
        installed.push(*name);
    }
    Ok(json!({
        "dir": dir.display().to_string(),
        "installed": installed,
        "unchanged": unchanged,
        "kept": kept,
    }))
}
