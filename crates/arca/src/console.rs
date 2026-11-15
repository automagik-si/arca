//! The console's token: how the machine's own console (Arca OS's screen, drawn by a root program) reads from the node.
//!
//! With `console.tokenFile` set, `serve` makes one viewer token at each start, limited to [`CONSOLE_TOOLS`], and holds it
//! in its token store's memory only ([`arca_mcp::TokenStore::hold`]), under the id [`CONSOLE_TOKEN_ID`]. Once the node holds
//! its unix socket, and before it serves there, the token is written to the file, created new at mode 0400 and never
//! through a link. A reload keeps the token, a restart replaces it (the earlier value authenticates nobody), and a stop
//! removes the file this process wrote. The console sends the token as any client does: the bearer rule is the same rule
//! on every route and every listener.
//!
//! The start is refused, in words, when the file's directory is not a directory this user owns and no group or other user
//! may enter, when a link is at the file's path, or when something other than this node's earlier file is there. A file
//! an earlier node left (a crash, a kill) is this user's regular 0400 file holding one dead token, and is replaced. The
//! token's value is never printed, logged or put in an error.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use arca_api::Scope;
use arca_mcp::TokenStore;

use crate::Failure;

/// The id the console's token has in `token_list`. It begins with [`arca_mcp::HELD_PREFIX`], so no hash file can carry it.
pub const CONSOLE_TOKEN_ID: &str = "node:console";

/// The reads the console's token may call, and no more: what the console's first screen needs.
pub const CONSOLE_TOOLS: [&str; 4] = ["node_metrics", "arca_status", "alerts_list", "service_list"];

/// What the config key is called, as every refusal names it.
const KEY: &str = "console.tokenFile";

/// The length of the file this node writes: a token (`arca_` and 43 characters) and a newline.
const FILE_LEN: u64 = 49;

#[cfg(target_os = "linux")]
const NOFOLLOW: i32 = 0o400000;
#[cfg(not(target_os = "linux"))]
const NOFOLLOW: i32 = 0;

/// The console's token of one running node: the file it goes to and, until it is written, the secret.
pub struct ConsoleToken {
    path: PathBuf,
    secret: String,
    /// The device and inode of the file this process wrote, so a stop removes that file and never another.
    written: OnceLock<(u64, u64)>,
}

impl ConsoleToken {
    /// Checks the place of the file at `path` for a node run by this process's user, then makes the console's token and
    /// has `tokens` hold it. Nothing is written yet ([`ConsoleToken::write`]).
    pub fn prepare(path: &Path, tokens: &TokenStore) -> Result<ConsoleToken, Failure> {
        check_place(path, own_uid())?;
        let tools = CONSOLE_TOOLS.iter().map(|tool| tool.to_string()).collect();
        let secret = tokens.hold(CONSOLE_TOKEN_ID, Scope::Viewer, tools);
        Ok(ConsoleToken {
            path: path.to_path_buf(),
            secret,
            written: OnceLock::new(),
        })
    }

    /// The file's path, which may be said; the token never is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes the token to the file: its place is checked again, a file an earlier node left is removed, and the file is
    /// created new, mode 0400, never through a link. An error names the file and the rule, never the token.
    pub fn write(&self) -> io::Result<()> {
        let refused = |failure: Failure| {
            io::Error::new(io::ErrorKind::InvalidInput, failure.message().to_string())
        };
        if check_place(&self.path, own_uid()).map_err(refused)? == Place::Leftover {
            fs::remove_file(&self.path)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o400)
            .custom_flags(NOFOLLOW)
            .open(&self.path)?;
        let written = file
            .write_all(format!("{}\n", self.secret).as_bytes())
            .and_then(|()| file.set_permissions(fs::Permissions::from_mode(0o400)))
            .and_then(|()| file.sync_all())
            .and_then(|()| file.metadata());
        match written {
            Ok(meta) => {
                let _ = self.written.set((meta.dev(), meta.ino()));
                Ok(())
            }
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(&self.path);
                Err(error)
            }
        }
    }

    /// Removes the file when it is still the one this process wrote. Anything else at the path is left alone.
    pub fn remove_own(&self) {
        let Some(own) = self.written.get() else {
            return;
        };
        if fs::symlink_metadata(&self.path).is_ok_and(|meta| (meta.dev(), meta.ino()) == *own) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// The user this process runs as: the node's user.
fn own_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// What is at the file's path when its directory passes.
#[derive(Debug, PartialEq, Eq)]
enum Place {
    /// Nothing: the file is created.
    Empty,
    /// A file an earlier node of this user left: it is replaced.
    Leftover,
}

/// The rules for the file at `path` of a node run by `uid`: its directory is a real directory (not a link) that `uid` owns
/// and no group or other user may enter or read, and its path holds nothing, or this node's earlier file. Anything else
/// refuses the start, in words that name the key and the path.
fn check_place(path: &Path, uid: u32) -> Result<Place, Failure> {
    let refused =
        |why: String| Failure::Config(format!("{KEY} {} is refused: {why}", path.display()));
    let dir = path
        .parent()
        .ok_or_else(|| refused("it names no directory".to_string()))?;
    let meta = fs::symlink_metadata(dir).map_err(|_| {
        refused(format!(
            "its directory {} does not exist; it must be a directory of the node's user, mode 0700",
            dir.display()
        ))
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(refused(format!(
            "its directory {} is not a directory (a link is not taken)",
            dir.display()
        )));
    }
    if meta.uid() != uid {
        return Err(refused(format!(
            "its directory {} is not owned by the user the node runs as",
            dir.display()
        )));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(refused(format!(
            "its directory {} is open to group or others; make it mode 0700",
            dir.display()
        )));
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Place::Empty),
        Err(error) => return Err(refused(format!("it cannot be examined ({error})"))),
    };
    if meta.file_type().is_symlink() {
        return Err(refused(
            "a link is at its path; the node never writes through a link, and leaves it alone"
                .to_string(),
        ));
    }
    if is_leftover(path, &meta, uid) {
        return Ok(Place::Leftover);
    }
    Err(refused(
        "something other than this node's earlier token file is at its path, and the node leaves it alone: remove it, or name another file"
            .to_string(),
    ))
}

/// Whether the file at `path` is one this node wrote before: a regular file of `uid`, mode 0400, one link, holding one
/// token's shape and its newline and nothing else.
fn is_leftover(path: &Path, meta: &fs::Metadata, uid: u32) -> bool {
    if !meta.is_file()
        || meta.uid() != uid
        || meta.mode() & 0o7777 != 0o400
        || meta.nlink() != 1
        || meta.len() != FILE_LEN
    {
        return false;
    }
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file.take(FILE_LEN + 1).read_to_end(&mut bytes).is_err() {
        return false;
    }
    bytes.len() as u64 == FILE_LEN
        && bytes.starts_with(b"arca_")
        && bytes.ends_with(b"\n")
        && bytes[5..bytes.len() - 1]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("arca-console-unit-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("dir");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("0700");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_directory_another_user_owns_refuses_the_start() {
        let scratch = Scratch::new("foreign-owner");
        let path = scratch.0.join("console.token");
        let other = own_uid().wrapping_add(1);
        let refused = check_place(&path, other).expect_err("another user's directory");
        let text = refused.message().to_string();
        assert!(
            text.contains(KEY) && text.contains("not owned by the user the node runs as"),
            "{text}"
        );
        assert_eq!(check_place(&path, own_uid()).expect("own"), Place::Empty);
    }

    #[test]
    fn a_missing_directory_refuses_the_start() {
        let scratch = Scratch::new("missing");
        let path = scratch.0.join("absent").join("console.token");
        let text = check_place(&path, own_uid())
            .expect_err("missing")
            .message()
            .to_string();
        assert!(text.contains("does not exist"), "{text}");
    }

    #[test]
    fn the_file_is_written_0400_its_token_is_held_and_a_stop_removes_only_this_processs_file() {
        let scratch = Scratch::new("write");
        let path = scratch.0.join("console.token");
        let tokens = TokenStore::load(&scratch.0).expect("an empty store");
        let console = ConsoleToken::prepare(&path, &tokens).expect("prepared");
        assert!(fs::symlink_metadata(&path).is_err(), "nothing before write");
        console.write().expect("written");
        let meta = fs::symlink_metadata(&path).expect("written");
        assert_eq!(meta.mode() & 0o7777, 0o400);
        let token = fs::read_to_string(&path).expect("read");
        let caller = tokens.lookup(token.trim_end()).expect("held");
        assert_eq!(caller.token_id, CONSOLE_TOKEN_ID);
        assert_eq!(caller.scope, Scope::Viewer);
        assert_eq!(
            caller.tools,
            Some(CONSOLE_TOOLS.iter().map(|t| t.to_string()).collect())
        );
        // Another file put in its place is not this process's, and a stop leaves it.
        fs::remove_file(&path).expect("remove");
        fs::write(&path, "other\n").expect("other");
        console.remove_own();
        assert_eq!(fs::read_to_string(&path).expect("kept"), "other\n");
    }

    #[test]
    fn a_leftover_must_be_this_users_0400_file_of_one_token_shape() {
        let scratch = Scratch::new("leftover");
        let path = scratch.0.join("console.token");
        let shaped = format!("arca_{}\n", "B".repeat(43));
        for (text, mode, expected) in [
            (shaped.as_str(), 0o400, true),
            (shaped.as_str(), 0o600, false),
            ("arca_short\n", 0o400, false),
            (&*format!("{}x", shaped.trim_end()), 0o400, false),
        ] {
            let _ = fs::remove_file(&path);
            fs::write(&path, text).expect("write");
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("mode");
            let meta = fs::symlink_metadata(&path).expect("meta");
            assert_eq!(
                is_leftover(&path, &meta, own_uid()),
                expected,
                "{mode:o} {text:?}"
            );
        }
        let _ = fs::remove_file(&path);
        fs::write(&path, &shaped).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).expect("mode");
        let meta = fs::symlink_metadata(&path).expect("meta");
        assert!(
            !is_leftover(&path, &meta, own_uid().wrapping_add(1)),
            "another user's file is no leftover of this node"
        );
    }
}
