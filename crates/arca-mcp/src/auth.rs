//! Interim bearer tokens: stored digests, a strict file format and one check per request.
//!
//! A token is the secret `arca_` followed by 43 base64url characters (32 random bytes). The server holds only the SHA-256
//! digest of the whole secret, in `<id>.hash`, whose text is `scope=<scope>` and `sha256=<64 lowercase hex>`, then, each at
//! most once and in any order, the optional `tools=<comma list>`, `services=<comma list>` ([`ServiceLimit`]),
//! `expires=<RFC 3339 UTC time>` and `by=<parent token id>`, each on its own line. A secret carries 256 random bits, so a
//! plain digest is enough: a copied hash file yields no usable secret.
//!
//! A line of the form `<key>=<value>` whose key this release does not know leaves that one token out: the store loads
//! without it, says so ([`TokenStore::skipped`]), and its secret authenticates nobody. The key may be a limit a later
//! release added, so the token is never taken without it, and never wider than it was minted. Any other line that does not
//! parse still refuses the whole directory. A release older than the one that added `services=`, `expires=` and `by=`
//! refuses the whole directory on any of them, so before such a downgrade every token with one of those lines is revoked.
//!
//! A token with `by=` was handed down by that token. It loads only while its parent does, and only when it is no wider:
//! no higher scope, no tool, service or later expiry its parent lacks. Revoking a parent so leaves out its whole chain at
//! the next load. An expired token is refused at each lookup.
//! A secret is never stored, logged or echoed, and an error names a file, never its content. The token directory is a real
//! directory, not a symlink, owned by this process and writable by its owner only.
//!
//! Besides the stored tokens, the store keeps the tokens this process holds itself ([`TokenStore::hold`]): made at start,
//! known only to this process's memory, never in the token directory, kept by a reload and gone with the process. Their ids
//! begin with [`HELD_PREFIX`], which no hash file's name can carry, so a held token never shadows a stored one.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use arca_api::Scope;
use arca_api::limits::ServiceLimit;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::fresh::process_uid;

const PREFIX: &str = "arca_";
const BODY_LEN: usize = 43;
const SECRET_BYTES: usize = 32;
/// The largest hash file the server reads. A well-formed one is about 100 bytes.
pub const MAX_FILE_BYTES: u64 = 4096;
/// The longest id a hash file may name.
const MAX_ID_LEN: usize = 64;

/// The identity a request authenticated as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub token_id: String,
    pub scope: Scope,
    /// The token's optional tool allowlist. `None` admits every verb the scope does.
    pub tools: Option<Vec<String>>,
    /// What else the token is limited to. The default limits nothing more.
    pub limits: Limits,
}

/// What a stored token is limited to besides its scope and its tool list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Limits {
    /// The services it may reach. `None` reaches every service its scope and tools do.
    pub services: Option<ServiceLimit>,
    /// When it stops being accepted, in seconds since the Unix epoch. `None` never.
    pub expires: Option<u64>,
    /// The id of the token that handed it down.
    pub by: Option<String>,
}

impl Caller {
    /// Whether this token's allowlist admits `tool`. A token without a list admits all of them.
    pub fn allows_tool(&self, tool: &str) -> bool {
        self.tools
            .as_ref()
            .is_none_or(|tools| tools.iter().any(|name| name == tool))
    }

    /// Whether this caller may call `verb`: its scope, its tool list, and for a verb a token reaches only by naming it
    /// ([`arca_api::verbs::NAMED_ONLY`]) an operator scope or a list that names it.
    pub fn admits(&self, verb: &arca_api::Verb) -> bool {
        arca_api::may_call_listed(self.scope, self.tools.as_deref(), verb)
    }
}

/// The prefix of the caller id of every token an authorization server issued ([`IssuedTokens`]). A stored token's id is
/// lowercase letters, digits and hyphens, so no static token can carry it.
pub const ISSUED_PREFIX: &str = "oauth:";

/// Tokens an OAuth authorization server issued for this MCP server (MCP authorization, 2025-11-25). The server asks it
/// about a bearer the static store does not know. An implementation answers only for a live token whose audience is this
/// server, and names the caller with [`ISSUED_PREFIX`] (Arca's: `oauth:<client id>/<grant id>`, one id per grant), so a
/// static token's id is never shadowed.
pub trait IssuedTokens: Send + Sync {
    /// The caller a bearer authenticates as, or `None`.
    fn lookup(&self, bearer: &str) -> Option<Caller>;
}

impl Caller {
    /// Whether this caller holds an issued (OAuth) token rather than a stored one.
    pub fn is_issued(&self) -> bool {
        self.token_id.starts_with(ISSUED_PREFIX)
    }
}

/// Why a token directory will not load. Each variant names one file, never the file's content. `UnsafeDir` names the
/// directory itself, with a fixed phrase for its reason: a symlink, a uid other than the server's, or a group or other
/// write bit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenStoreError {
    NotDir,
    UnsafeDir { reason: &'static str },
    BadMode { file: String },
    Symlink { file: String },
    NotOwner { file: String },
    Malformed { file: String },
    Io { file: String },
}

impl std::fmt::Display for TokenStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenStoreError::NotDir => {
                write!(f, "the token directory is missing or not a directory")
            }
            TokenStoreError::UnsafeDir { reason } => write!(f, "the token directory {reason}"),
            TokenStoreError::BadMode { file } => write!(f, "token file {file} is not mode 0400"),
            TokenStoreError::Symlink { file } => write!(f, "token file {file} is a symlink"),
            TokenStoreError::NotOwner { file } => {
                write!(f, "token file {file} is not owned by the server")
            }
            TokenStoreError::Malformed { file } => write!(f, "token file {file} is malformed"),
            TokenStoreError::Io { file } => write!(f, "cannot read {file}"),
        }
    }
}

impl std::error::Error for TokenStoreError {}

/// One loaded token: its id, its scope, its optional tool allowlist, its other limits and the digest of its secret.
#[derive(Clone)]
struct Entry {
    id: String,
    scope: Scope,
    tools: Option<Vec<String>>,
    limits: Limits,
    digest: [u8; 32],
}

/// Why a hash file was left out of a load that went on without it ([`TokenStore::skipped`]).
pub const SKIP_UNKNOWN_LINE: &str =
    "it has a line this release does not know, which may be a limit it cannot keep";
/// See [`SKIP_UNKNOWN_LINE`].
pub const SKIP_ADMIN_LIMITED: &str = "an admin token is never limited to services";
/// See [`SKIP_UNKNOWN_LINE`].
pub const SKIP_NO_PARENT: &str = "the token that handed it down is not loaded";
/// See [`SKIP_UNKNOWN_LINE`].
pub const SKIP_WIDER: &str = "it is wider than the token that handed it down";

/// The prefix of the id of every token this process holds and never stores ([`TokenStore::hold`]). A stored token's id is
/// lowercase letters, digits and hyphens, so no hash file can carry it.
pub const HELD_PREFIX: &str = "node:";

/// The loaded token digests, read once at start and replaceable on SIGHUP, and the tokens this process holds itself, which
/// a reload keeps. Cloning shares the live set.
#[derive(Clone)]
pub struct TokenStore {
    entries: Arc<RwLock<Vec<Entry>>>,
    /// The tokens this process made and holds in memory only ([`TokenStore::hold`]). A reload never touches them.
    held: Arc<RwLock<Vec<Entry>>>,
    /// The hash files the last load left out ([`TokenStore::ignored`]).
    ignored: Arc<RwLock<Vec<String>>>,
    /// The other hash files the last load left out, each with why ([`TokenStore::skipped`]).
    skipped: Arc<RwLock<Vec<(String, &'static str)>>>,
}

impl std::fmt::Debug for TokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenStore")
            .field(
                "tokens",
                &self.entries.read().map(|e| e.len()).unwrap_or_default(),
            )
            .field(
                "held",
                &self.held.read().map(|e| e.len()).unwrap_or_default(),
            )
            .finish()
    }
}

impl TokenStore {
    /// Loads every hash file of `dir`, a private directory of this process's uid whose files are owned by that uid too.
    pub fn load(dir: &Path) -> Result<TokenStore, TokenStoreError> {
        let uid = process_uid().map_err(|_| TokenStoreError::Io {
            file: "the uid probe".into(),
        })?;
        TokenStore::load_as(dir, uid)
    }

    /// Loads every hash file of `dir`, each of which must be owned by `owner_uid`. [`TokenStore::load`] passes this
    /// process's uid.
    ///
    /// Test seam: a test names a uid other than its own to reach the file-owner refusal without root. The directory is
    /// always checked against this process's uid, never against `owner_uid`.
    #[doc(hidden)]
    pub fn load_as(dir: &Path, owner_uid: u32) -> Result<TokenStore, TokenStoreError> {
        check_dir(dir)?;
        let mut paths = list(dir)?;
        paths.sort();
        let mut entries: Vec<Entry> = Vec::with_capacity(paths.len());
        let mut ignored = Vec::new();
        let mut skipped = Vec::new();
        for path in paths {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // A file under the name approved actions run under is no token. An older release let one be minted, so
            // it is left out, not refused: the store loads without it, and its secret authenticates nobody.
            if name.strip_suffix(".hash") == Some(APPROVED_ACTION_ID) {
                ignored.push(name);
                continue;
            }
            let id = name
                .strip_suffix(".hash")
                .filter(|id| valid_id(id))
                .ok_or_else(|| TokenStoreError::Malformed { file: name.clone() })?
                .to_string();
            let entry = match load_one(&path, &name, id, owner_uid)? {
                Loaded::Entry(entry) => entry,
                Loaded::Skipped(why) => {
                    skipped.push((name, why));
                    continue;
                }
            };
            if entries
                .iter()
                .any(|e| digests_equal(&e.digest, &entry.digest))
            {
                // Two files holding one secret would make a lookup depend on file order, so the second is refused.
                return Err(TokenStoreError::Malformed { file: name });
            }
            entries.push(entry);
        }
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        for (id, why) in follow_chains(&mut entries) {
            skipped.push((format!("{id}.hash"), why));
        }
        skipped.sort();
        Ok(TokenStore {
            entries: Arc::new(RwLock::new(entries)),
            held: Arc::new(RwLock::new(Vec::new())),
            ignored: Arc::new(RwLock::new(ignored)),
            skipped: Arc::new(RwLock::new(skipped)),
        })
    }

    /// The other hash files the last load, or reload, left out, by file name, each with why: a line this release does not
    /// know, an admin token limited to services, or a token handed down whose parent is not loaded or that is wider than
    /// it. The store loaded without them, and their secrets authenticate nobody.
    pub fn skipped(&self) -> Vec<(String, &'static str)> {
        self.skipped
            .read()
            .map(|skipped| skipped.clone())
            .unwrap_or_default()
    }

    /// The hash files the last load, or reload, found and left out: a file named for [`APPROVED_ACTION_ID`]. Whoever
    /// loaded the store says so once, since such a file is the owner's to remove.
    pub fn ignored(&self) -> Vec<String> {
        self.ignored
            .read()
            .map(|ignored| ignored.clone())
            .unwrap_or_default()
    }

    /// Replaces the live set of stored tokens from `dir` and returns its size. The previous set stays in place when the load
    /// fails. The tokens this process holds ([`TokenStore::hold`]) are not in `dir`, and a reload keeps them.
    pub fn reload(&self, dir: &Path) -> Result<usize, TokenStoreError> {
        let loaded = TokenStore::load(dir)?;
        let next = loaded
            .entries
            .read()
            .map_err(|_| TokenStoreError::Io {
                file: "the token store".into(),
            })?
            .clone();
        let len = next.len();
        let left_out = loaded.ignored();
        let skipped = loaded.skipped();
        *self.entries.write().map_err(|_| TokenStoreError::Io {
            file: "the token store".into(),
        })? = next;
        if let Ok(mut ignored) = self.ignored.write() {
            *ignored = left_out;
        }
        if let Ok(mut slot) = self.skipped.write() {
            *slot = skipped;
        }
        Ok(len)
    }

    /// The caller a bearer authenticates as, or `None`. Every stored and held digest is compared, with no early exit. A
    /// token past its `expires` authenticates nobody.
    pub fn lookup(&self, bearer: &str) -> Option<Caller> {
        self.lookup_at(bearer, unix_now())
    }

    /// [`TokenStore::lookup`] at `now`, in seconds since the Unix epoch.
    pub fn lookup_at(&self, bearer: &str, now: u64) -> Option<Caller> {
        if !well_formed(bearer) {
            return None;
        }
        let digest = sha256(bearer.as_bytes());
        let entries = self.entries.read().ok()?;
        let held = self.held.read().ok()?;
        let mut found = None;
        for entry in entries.iter().chain(held.iter()) {
            if digests_equal(&entry.digest, &digest) {
                found = Some(entry);
            }
        }
        let found = found?;
        if found.limits.expires.is_some_and(|expires| now >= expires) {
            return None;
        }
        Some(Caller {
            token_id: found.id.clone(),
            scope: found.scope,
            tools: found.tools.clone(),
            limits: found.limits.clone(),
        })
    }

    /// Makes a new secret of `scope`, limited to `tools`, that this process holds under `id` in memory only, and returns it.
    /// Nothing is written to the token directory: the caller decides where the secret goes. A reload keeps the token, and
    /// it ends with the process. A second call with the same id replaces the first token, whose secret then authenticates
    /// nobody.
    ///
    /// # Panics
    ///
    /// Panics when `id` does not begin with [`HELD_PREFIX`]: an id a hash file could carry would let a stored token and a
    /// held one share a name, a wiring error found at start.
    pub fn hold(&self, id: &str, scope: Scope, tools: Vec<String>) -> String {
        assert!(
            id.starts_with(HELD_PREFIX) && id.len() > HELD_PREFIX.len(),
            "a held token's id begins with {HELD_PREFIX}, which no hash file can carry: {id}"
        );
        let (secret, _) = mint(scope);
        let entry = Entry {
            id: id.to_string(),
            scope,
            tools: Some(tools),
            limits: Limits::default(),
            digest: sha256(secret.as_bytes()),
        };
        let mut held = self
            .held
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.retain(|e| e.id != id);
        held.push(entry);
        held.sort_by(|a, b| a.id.cmp(&b.id));
        secret
    }

    /// Every token this process holds and never stores ([`TokenStore::hold`]): its id, scope and tool list, by id. Never
    /// a digest or a secret.
    pub fn held_ids(&self) -> Vec<(String, Scope, Option<Vec<String>>)> {
        self.held
            .read()
            .map(|held| {
                held.iter()
                    .map(|e| (e.id.clone(), e.scope, e.tools.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every stored token's id, scope and optional tool list, by id. Never a digest or a secret. The tokens this process
    /// holds are [`TokenStore::held_ids`].
    pub fn ids(&self) -> Vec<(String, Scope, Option<Vec<String>>)> {
        self.entries
            .read()
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| (e.id.clone(), e.scope, e.tools.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The ids of the hash files in `dir` whose scope is `admin`, sorted. A node holds at most one admin token, and `token mint`
/// asks this before it writes a second. A file that cannot be read or does not parse is not counted: the store's own load
/// refuses it with a word of its own.
pub fn admin_token_ids(dir: &Path) -> Vec<String> {
    let mut ids: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|item| {
            let name = item.file_name().to_string_lossy().into_owned();
            let id = name
                .strip_suffix(".hash")
                .filter(|id| valid_id(id))?
                .to_string();
            let mut bytes = Vec::new();
            File::open(item.path())
                .ok()?
                .take(MAX_FILE_BYTES)
                .read_to_end(&mut bytes)
                .ok()?;
            let text = std::str::from_utf8(&bytes).ok()?;
            let Ok(Body { scope, limits, .. }) = parse_body(text) else {
                return None;
            };
            (scope == Scope::Admin && limits.services.is_none()).then_some(id)
        })
        .collect();
    ids.sort();
    ids
}

/// A new secret of `scope`, and the text of its hash file.
pub fn mint(scope: Scope) -> (String, String) {
    mint_with_tools(scope, None)
}

/// A new secret of `scope` and its hash-file text, with an optional tool allowlist as the third line.
pub fn mint_with_tools(scope: Scope, tools: Option<&[String]>) -> (String, String) {
    mint_limited(scope, tools, &Limits::default())
}

/// A new secret of `scope` and its hash-file text, with an optional tool allowlist and the `limits` it is minted with:
/// the `services=`, `expires=` and `by=` lines, each when it is set.
pub fn mint_limited(scope: Scope, tools: Option<&[String]>, limits: &Limits) -> (String, String) {
    let mut raw = [0u8; SECRET_BYTES];
    getrandom::fill(&mut raw).expect("the operating system's random source is available");
    let secret = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(raw));
    let mut text = format!(
        "scope={}\nsha256={}\n",
        scope.as_str(),
        to_hex(&sha256(secret.as_bytes()))
    );
    if let Some(tools) = tools {
        text.push_str(&format!("tools={}\n", tools.join(",")));
    }
    if let Some(services) = &limits.services {
        text.push_str(&format!("services={}\n", services.as_line()));
    }
    if let Some(expires) = limits.expires {
        text.push_str(&format!("expires={}\n", format_rfc3339(expires)));
    }
    if let Some(by) = &limits.by {
        text.push_str(&format!("by={by}\n"));
    }
    (secret, text)
}

/// The token directory itself: a real directory, not a symlink, owned by this process's uid and not writable by a group or
/// by others. A path that is missing or is not a directory is `NotDir`.
fn check_dir(dir: &Path) -> Result<(), TokenStoreError> {
    let meta = fs::symlink_metadata(dir).map_err(|_| TokenStoreError::NotDir)?;
    if meta.file_type().is_symlink() {
        return Err(TokenStoreError::UnsafeDir {
            reason: "is a symlink",
        });
    }
    if !meta.is_dir() {
        return Err(TokenStoreError::NotDir);
    }
    let uid = process_uid().map_err(|_| TokenStoreError::Io {
        file: "the uid probe".into(),
    })?;
    if meta.uid() != uid {
        return Err(TokenStoreError::UnsafeDir {
            reason: "is not owned by the server",
        });
    }
    if meta.mode() & 0o022 != 0 {
        return Err(TokenStoreError::UnsafeDir {
            reason: "is writable by a group or by others",
        });
    }
    Ok(())
}

fn list(dir: &Path) -> Result<Vec<PathBuf>, TokenStoreError> {
    let read = fs::read_dir(dir).map_err(|_| TokenStoreError::Io {
        file: "the token directory".into(),
    })?;
    let mut paths = Vec::new();
    for item in read {
        let item = item.map_err(|_| TokenStoreError::Io {
            file: "the token directory".into(),
        })?;
        paths.push(item.path());
    }
    Ok(paths)
}

/// What one hash file holds: a token, or one the load leaves out and says why ([`TokenStore::skipped`]).
enum Loaded {
    Entry(Entry),
    Skipped(&'static str),
}

/// One hash file, checked before it is read: a regular file (not a symlink, not one swapped for a symlink between the
/// checks), mode exactly 0400, owned by `owner_uid`, at most `MAX_FILE_BYTES`, and well formed.
fn load_one(
    path: &Path,
    name: &str,
    id: String,
    owner_uid: u32,
) -> Result<Loaded, TokenStoreError> {
    let io_err = || TokenStoreError::Io {
        file: name.to_string(),
    };
    let link = fs::symlink_metadata(path).map_err(|_| io_err())?;
    if link.file_type().is_symlink() {
        return Err(TokenStoreError::Symlink {
            file: name.to_string(),
        });
    }
    if !link.file_type().is_file() {
        return Err(TokenStoreError::Malformed {
            file: name.to_string(),
        });
    }
    let mut file = File::open(path).map_err(|_| io_err())?;
    let meta = file.metadata().map_err(|_| io_err())?;
    if (meta.dev(), meta.ino()) != (link.dev(), link.ino()) {
        return Err(TokenStoreError::Symlink {
            file: name.to_string(),
        });
    }
    if meta.permissions().mode() & 0o7777 != 0o400 {
        return Err(TokenStoreError::BadMode {
            file: name.to_string(),
        });
    }
    if meta.uid() != owner_uid {
        return Err(TokenStoreError::NotOwner {
            file: name.to_string(),
        });
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| io_err())?;
    let malformed = || TokenStoreError::Malformed {
        file: name.to_string(),
    };
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(malformed());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| malformed())?;
    let body = match parse_body(text) {
        Ok(body) => body,
        Err(BodyError::Unknown) => return Ok(Loaded::Skipped(SKIP_UNKNOWN_LINE)),
        Err(BodyError::Malformed) => return Err(malformed()),
    };
    if body.limits.by.as_deref() == Some(id.as_str()) {
        return Err(malformed());
    }
    if body.scope == Scope::Admin && body.limits.services.is_some() {
        return Ok(Loaded::Skipped(SKIP_ADMIN_LIMITED));
    }
    Ok(Loaded::Entry(Entry {
        id,
        scope: body.scope,
        tools: body.tools,
        limits: body.limits,
        digest: body.digest,
    }))
}

/// Leaves out every token handed down (`by=`) whose parent is not loaded, or that is wider than its parent, until none
/// is left: a child of a token left out goes too, and so does every token of a loop. Returns each id left out, with why.
fn follow_chains(entries: &mut Vec<Entry>) -> Vec<(String, &'static str)> {
    let mut left_out = Vec::new();
    loop {
        let bad = entries.iter().find_map(|child| {
            let by = child.limits.by.as_deref()?;
            let Some(parent) = entries.iter().find(|e| e.id == by) else {
                return Some((child.id.clone(), SKIP_NO_PARENT));
            };
            if !no_wider(child, parent) {
                return Some((child.id.clone(), SKIP_WIDER));
            }
            // A chain that comes back to a token it passed is a loop, and no token of it was handed down by a root.
            let mut seen = vec![child.id.as_str()];
            let mut at = parent;
            while let Some(up) = at.limits.by.as_deref() {
                if seen.contains(&at.id.as_str()) {
                    return Some((child.id.clone(), SKIP_NO_PARENT));
                }
                seen.push(at.id.as_str());
                at = entries.iter().find(|e| e.id == up)?;
            }
            None
        });
        let Some((id, why)) = bad else {
            return left_out;
        };
        entries.retain(|e| e.id != id);
        left_out.push((id, why));
    }
}

/// Whether `child` is no wider than `parent`: no higher scope, and every tool, service and moment it admits, its parent
/// admits too.
fn no_wider(child: &Entry, parent: &Entry) -> bool {
    let tools = match (&child.tools, &parent.tools) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(mine), Some(theirs)) => mine.iter().all(|tool| theirs.contains(tool)),
    };
    let services = match (&child.limits.services, &parent.limits.services) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(mine), Some(theirs)) => mine.within(theirs),
    };
    let expires = match (child.limits.expires, parent.limits.expires) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(mine), Some(theirs)) => mine <= theirs,
    };
    child.scope <= parent.scope && tools && services && expires
}

/// The lines of one hash file.
struct Body {
    scope: Scope,
    tools: Option<Vec<String>>,
    limits: Limits,
    digest: [u8; 32],
}

/// Why a hash file's text is not a token this release takes.
#[derive(Debug, PartialEq, Eq)]
enum BodyError {
    /// It does not parse: the whole directory is refused, as ever.
    Malformed,
    /// It parses, and has a `<key>=<value>` line whose key this release does not know: the token is left out.
    Unknown,
}

/// The lines of a hash file: `scope=` and `sha256=` first, then each optional line at most once, in any order. LF endings
/// and a final newline. A file of the first two lines admits every verb the scope does, on every service, for ever.
fn parse_body(text: &str) -> Result<Body, BodyError> {
    use BodyError::Malformed;
    let body = text.strip_suffix('\n').ok_or(Malformed)?;
    let mut lines = body.split('\n');
    let mut next = |key: &str| {
        lines
            .next()
            .and_then(|line| line.strip_prefix(key))
            .ok_or(Malformed)
    };
    let scope = parse_scope(next("scope=")?).ok_or(Malformed)?;
    let digest = decode_hex(next("sha256=")?).ok_or(Malformed)?;
    let mut tools = None;
    let mut limits = Limits::default();
    let mut unknown = false;
    let mut seen: Vec<&str> = Vec::new();
    for line in lines {
        let (key, value) = line.split_once('=').ok_or(Malformed)?;
        if seen.contains(&key) {
            return Err(Malformed);
        }
        seen.push(key);
        match key {
            "tools" => {
                tools = Some(
                    value
                        .split(',')
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                        .collect(),
                )
            }
            "services" => limits.services = Some(ServiceLimit::parse(value).ok_or(Malformed)?),
            "expires" => limits.expires = Some(parse_rfc3339(value).ok_or(Malformed)?),
            "by" => {
                limits.by = Some(
                    Some(value)
                        .filter(|by| valid_id(by))
                        .ok_or(Malformed)?
                        .to_string(),
                )
            }
            key if future_key(key) => unknown = true,
            _ => return Err(Malformed),
        }
    }
    if unknown {
        return Err(BodyError::Unknown);
    }
    Ok(Body {
        scope,
        tools,
        limits,
        digest,
    })
}

/// A key a later release may add to a hash file: 1 to 32 lowercase letters, digits and hyphens, starting with a letter.
fn future_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Seconds since the Unix epoch, now.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A moment as a token file writes it: RFC 3339 in UTC, to the second (`2026-11-15T18:00:00Z`).
pub fn format_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// The moment a token file's `expires=` names, in seconds since the Unix epoch: RFC 3339 in UTC, to the second, exactly
/// as [`format_rfc3339`] writes it, from 1970 to 9999. Any other spelling, an offset or a fraction among them, is `None`.
pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let b = text.as_bytes();
    let shape = b.len() == 20
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'Z';
    if !shape {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<u64> {
        let part = &text[from..to];
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if year < 1970 || !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day < 1 || day > month_days[(month - 1) as usize] {
        return None;
    }
    let days = days_from_civil(year as i64, month as u32, day as u32);
    u64::try_from(days)
        .ok()
        .map(|days| days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day since 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn parse_scope(text: &str) -> Option<Scope> {
    match text {
        "viewer" => Some(Scope::Viewer),
        "doctor" => Some(Scope::Doctor),
        "operator" => Some(Scope::Operator),
        "admin" => Some(Scope::Admin),
        _ => None,
    }
}

/// The caller id an action runs under once the owner approved its proposal. It is not a token: no token may be
/// minted under it, so a call under this id is always an approved action and never a person's token.
pub const APPROVED_ACTION_ID: &str = "owner-approval";

/// An id is 1 to 64 characters: a lowercase letter or digit, then lowercase letters, digits and hyphens.
fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let first_ok = bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    first_ok
        && bytes.len() <= MAX_ID_LEN
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// A bearer is refused before any hash is computed unless it has the shape of a secret.
fn well_formed(bearer: &str) -> bool {
    bearer.len() == PREFIX.len() + BODY_LEN
        && bearer.starts_with(PREFIX)
        && bearer[PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn decode_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        *slot = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Equal digests, decided by the bits of both: no early exit on the first differing byte.
fn digests_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}
