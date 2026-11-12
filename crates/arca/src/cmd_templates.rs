//! `templates install [--config <file>] [--replace]`: puts the shipped service templates in `<stateDir>/templates`, each
//! template's JSON together with the script its entrypoint or its first boot reads. The files are the repository's `templates/` as this build
//! holds them, so no checkout is needed and a template never arrives without its script.
//!
//! A file that is missing is written. A file that is already the shipped one is left. A file that is, byte for byte, a
//! version an earlier release shipped ([`EARLIER`]) is brought up to this release's and named under `replaced`: nobody
//! edited it, so nothing of the owner's is lost, and a node keeps no template an upgrade has since fixed. Any other file
//! that differs is kept, since it is the owner's edit, unless `--replace` is given. A template the owner added under
//! another name is never touched.
//!
//! A template and the script it reads move together. Where the owner edited one of them, the other is kept as it is too,
//! though it is an earlier shipped version: a new template is never paired with a script it was not shipped with.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use serde_json::{Value, json};

use crate::{Failure, Flags};

macro_rules! shipped {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/", $name)))),*]
    };
}

/// Every file a shipped template needs on a node: each template's JSON and the script its entrypoint or first boot names. The image build
/// inputs (`automagik.build.sh`, `automagik-dsh/`) are used from a checkout and are not installed.
pub const SHIPPED: &[(&str, &str)] = shipped![
    "automagik-server.json",
    "automagik-workspace.json",
    "automagik-workspace.sh",
    "automagik.json",
    "cloudflare-tunnel.json",
    "github-runner-pool.json",
    "github-runner-pool.sh",
    "gitlab-runner-pool-large.json",
    "gitlab-runner-pool.json",
    "gitlab-runner-pool.sh",
    "nats.json",
    "omni-api.json",
    "postgres-autopg.json",
    "postgres-autopg.sh",
    "workspace-vm.json",
];

/// The versions of [`SHIPPED`] files that earlier releases shipped and this one changed: each file's name and the
/// SHA-256 of its whole content. An installed file that matches one is replaced without `--replace`.
///
/// **When a release changes a shipped file, add the hash of the version the previous release shipped** (`git show
/// v<previous>:templates/<name> | sha256sum`). A test checks the list against the release tags where the checkout has
/// them. A hash is of a file exactly as a release shipped it, so an owner's edit never matches one.
pub const EARLIER: &[(&str, &str)] = &[
    // 0.8.0 and 0.9.0.
    (
        "automagik-workspace.json",
        "658fc9eb7fc4337cea1fcf891c4fe3db124b973ce5ec0bcebef1a97ebba64085",
    ),
    // 0.8.0 to 0.13.0: its keeper took the stop signal in the process that kept the workspace, where bash 5.2 can
    // lose it.
    (
        "automagik-workspace.sh",
        "8653ffd10e0c6962306818e74cfdfdd1b18f3d20828cae7eba17d6f47a801b90",
    ),
    // 0.8.0 to 0.12.0: before a pool obeyed a stop in every loop.
    (
        "github-runner-pool.sh",
        "b092ba0ec712f9a90cd3ed2c8fa6cac24e70de27c519e0c7ec09a952a4b502e2",
    ),
    // 0.13.0: before each pass looked at every slot once.
    (
        "github-runner-pool.sh",
        "0c41006927d037d304ba27c26aad983616f1178bf8d42b246f8d7b0817015bb4",
    ),
    // 0.9.0 to 0.12.0: the same.
    (
        "gitlab-runner-pool.sh",
        "9792af0cb048e8efcce28859eb4e4063ba479d52675d6cd074bef7954ac5204f",
    ),
    // 0.13.0: its controller took the stop signal in the process that ran the pool, where bash 5.2 can lose it.
    (
        "gitlab-runner-pool.sh",
        "e92d6a12e269866a0e9f89e1523bb428ead6d75d2cab6fb267d71f88f2c401f7",
    ),
    // 0.1.0-alpha.7 to 0.12.0: it declared POSTGRES_PASSWORD and did not make it, so a create came up stopped.
    (
        "postgres-autopg.json",
        "9d3740130e5a2a0eb48eeb733272ee425a09341bea197a09f74bedbd21db9187",
    ),
    // 0.1.0-alpha.7 to 0.12.0.
    (
        "postgres-autopg.sh",
        "955bd33f4123a6e62b2aaf4f2d91308a5d3888a2142eb2919447684586399c2e",
    ),
    // 0.13.0: its message for a missing password named `--replace`, which an unedited template no longer needs.
    (
        "postgres-autopg.sh",
        "226293b22c4dc25d1e207d738538170217f531293c15e19065ce2a319897a750",
    ),
];

/// What one installed file is, beside the shipped one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    /// It is not there.
    Missing,
    /// It is the shipped file.
    Current,
    /// It is a version an earlier release shipped, unedited.
    Earlier,
    /// It differs from every shipped version: the owner's edit.
    Edited,
}

/// The files that move together: each shipped template with every shipped script its text names. Two templates that
/// name the same script are one group.
fn groups() -> Vec<Vec<&'static str>> {
    let mut groups: Vec<Vec<&'static str>> = Vec::new();
    for (name, text) in SHIPPED.iter().filter(|(name, _)| name.ends_with(".json")) {
        let mut group: Vec<&'static str> = vec![name];
        group.extend(SHIPPED.iter().map(|(script, _)| *script).filter(|script| {
            !script.ends_with(".json") && text.contains(&format!("\"{script}\""))
        }));
        // Whatever shares a file with it is the same group.
        let (shared, apart): (Vec<_>, Vec<_>) = groups
            .into_iter()
            .partition(|other| other.iter().any(|file| group.contains(file)));
        groups = apart;
        for other in shared {
            for file in other {
                if !group.contains(&file) {
                    group.push(file);
                }
            }
        }
        groups.push(group);
    }
    groups
}

pub fn install(flags: &Flags) -> Result<Option<Value>, Failure> {
    // With --replace a template the owner edited is overwritten, so the node is named whole before it is.
    let does = if flags.switch("replace") {
        crate::Does::Removes
    } else {
        crate::Does::Acts
    };
    let (_, cfg) = crate::load_config_to(flags, does)?;
    install_into(&cfg.state_dir.join("templates"), flags.switch("replace")).map(Some)
}

/// Installs [`SHIPPED`] into `dir`, which is made (mode 0700) when it is missing. Answers the directory and each file's
/// name under `installed` (it was missing, or `replace` overwrote an edit), `unchanged`, `kept` (it differs and stays)
/// or `replaced` (it was an earlier release's own file, unedited, and is now this release's).
pub fn install_into(dir: &Path, replace: bool) -> Result<Value, Failure> {
    install_with(dir, replace, &mut |from, to| fs::rename(from, to))
}

/// [`install_into`], with the step that puts one written file in its place given by the caller: the tests make it
/// fail part-way.
fn install_with(
    dir: &Path,
    replace: bool,
    put: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<Value, Failure> {
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
    // What is there, before anything is written.
    let mut held = Vec::with_capacity(SHIPPED.len());
    for (name, text) in SHIPPED {
        let path = dir.join(name);
        held.push(match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.is_file() => {
                return Err(Failure::Refused(format!(
                    "{} is not a regular file; it is left alone and nothing is installed",
                    path.display()
                )));
            }
            Ok(_) => {
                let bytes = fs::read(&path)
                    .map_err(|error| failed("an installed template cannot be read", error))?;
                if bytes == text.as_bytes() {
                    Held::Current
                } else {
                    let digest = arca_kernel::canon::Digest::of(&bytes).to_hex();
                    if EARLIER
                        .iter()
                        .any(|(earlier, hash)| earlier == name && *hash == digest)
                    {
                        Held::Earlier
                    } else {
                        Held::Edited
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Held::Missing,
            Err(error) => return Err(failed("an installed template cannot be read", error)),
        });
    }
    let state = |name: &str| {
        SHIPPED
            .iter()
            .position(|(shipped, _)| *shipped == name)
            .map(|at| held[at])
    };
    // A file the owner edited holds its whole group as it is: the earlier shipped files beside it are kept too.
    let edited_groups: Vec<Vec<&'static str>> = groups()
        .into_iter()
        .filter(|group| group.iter().any(|file| state(file) == Some(Held::Edited)))
        .collect();
    let beside_an_edit = |name: &str| edited_groups.iter().any(|group| group.contains(&name));

    let (mut installed, mut unchanged, mut kept, mut replaced) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut to_write = Vec::new();
    for ((name, text), held) in SHIPPED.iter().zip(&held) {
        let path = dir.join(name);
        match held {
            Held::Current => {
                unchanged.push(*name);
                continue;
            }
            Held::Edited if !replace => {
                kept.push(*name);
                continue;
            }
            Held::Earlier if !replace && beside_an_edit(name) => {
                kept.push(*name);
                continue;
            }
            Held::Missing | Held::Edited | Held::Earlier => {}
        }
        to_write.push((*name, *text, path, *held == Held::Earlier));
    }

    // Every file is written beside its place first, and only then is each put in its place, so a reader never sees
    // half a file, and a write that fails (a full disk) fails before anything has changed. What is left is the
    // moment between two renames: a failure there has already put some files in place, and it names them.
    let scratch_of = |name: &str| dir.join(format!(".{name}.{}.tmp", std::process::id()));
    for (at, (name, text, _, _)) in to_write.iter().enumerate() {
        let scratch = scratch_of(name);
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&scratch)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            });
        if let Err(error) = written {
            // Only what this run made is taken away: a file that was already there under that name is not ours.
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                let _ = fs::remove_file(&scratch);
            }
            for (earlier, _, _, _) in &to_write[..at] {
                let _ = fs::remove_file(scratch_of(earlier));
            }
            return Err(Failure::Runtime(format!(
                "a template cannot be written ({}). Nothing was put in place: every installed template is as it was",
                error.kind()
            )));
        }
    }
    for (at, (name, _, path, earlier)) in to_write.iter().enumerate() {
        if let Err(error) = put(&scratch_of(name), path) {
            for (later, _, _, _) in &to_write[at..] {
                let _ = fs::remove_file(scratch_of(later));
            }
            let done: Vec<&str> = to_write[..at].iter().map(|(name, _, _, _)| *name).collect();
            return Err(Failure::Runtime(if done.is_empty() {
                format!(
                    "a template cannot be put in place ({}). Nothing was put in place: every installed template is as it was",
                    error.kind()
                )
            } else {
                format!(
                    "a template cannot be put in place ({}). Already put in place by this run: {}. The rest are as they were, so a template and the script it reads may be of two releases until this is run again, which finishes it",
                    error.kind(),
                    done.join(", ")
                )
            }));
        }
        if *earlier {
            replaced.push(*name);
        } else {
            installed.push(*name);
        }
    }
    Ok(json!({
        "dir": dir.display().to_string(),
        "installed": installed,
        "unchanged": unchanged,
        "kept": kept,
        "replaced": replaced,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arca-templates-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos())
        ));
        fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("the directory")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// A step that fails between two files has already put some in place. It says which, and that a template and its
    /// script may be of two releases until the next run; it leaves no half-written file behind; and the next run
    /// finishes the work.
    #[test]
    fn a_failure_part_way_names_what_is_already_in_place_and_the_next_run_finishes() {
        let dir = scratch("part-way").join("templates");
        let mut calls = 0;
        let refused = install_with(&dir, false, &mut |from, to| {
            calls += 1;
            if calls == 3 {
                return Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
            }
            fs::rename(from, to)
        });
        let Err(Failure::Runtime(why)) = refused else {
            panic!("the step fails");
        };
        let (first, second) = (SHIPPED[0].0, SHIPPED[1].0);
        assert_eq!(
            why,
            format!(
                "a template cannot be put in place ({}). Already put in place by this run: {first}, {second}. The rest are as they were, so a template and the script it reads may be of two releases until this is run again, which finishes it",
                std::io::ErrorKind::StorageFull
            )
        );
        // The two it names are there, whole; nothing else is, and no file written beside its place is left.
        assert_eq!(names(&dir), [first, second]);
        assert_eq!(
            fs::read_to_string(dir.join(first)).expect("the file"),
            SHIPPED[0].1
        );

        let finished = install_into(&dir, false).ok().expect("the next run");
        assert_eq!(finished["unchanged"], json!([first, second]));
        assert_eq!(
            finished["installed"].as_array().map(Vec::len),
            Some(SHIPPED.len() - 2)
        );
    }

    /// A step that cannot write one of its files has changed nothing yet, and says so.
    #[test]
    fn a_file_that_cannot_be_written_stops_the_step_before_anything_is_put_in_place() {
        let dir = scratch("unwritten").join("templates");
        fs::create_dir_all(&dir).expect("the templates directory");
        // The place a later file would be written to is taken by something that is not this run's.
        let taken = dir.join(format!(".{}.{}.tmp", SHIPPED[4].0, std::process::id()));
        fs::write(&taken, "not this run's").expect("a file in the way");
        let Err(Failure::Runtime(why)) = install_into(&dir, false) else {
            panic!("the step fails");
        };
        assert!(
            why.starts_with("a template cannot be written (")
                && why.ends_with("Nothing was put in place: every installed template is as it was"),
            "{why}"
        );
        assert_eq!(
            names(&dir),
            [taken.file_name().expect("a name").to_string_lossy()],
            "no template was put in place, the four files written before it were taken away, and the file in the way is left"
        );
        assert_eq!(
            fs::read_to_string(&taken).expect("the file in the way"),
            "not this run's"
        );
    }
}
