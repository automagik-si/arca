//! Interim bearer tokens: stored digests, a strict file format and one check per request.
//!
//! A token is the secret `arca_` followed by 43 base64url characters (32 random bytes). The server holds only the SHA-256
//! digest of the whole secret, in `<id>.hash`, whose text is `scope=<scope>`, `sha256=<64 lowercase hex>` and an optional
//! `tools=<comma list>`, each on its own line. A secret carries 256 random bits, so a plain digest is enough: a copied hash file yields no usable secret.
//! A secret is never stored, logged or echoed, and an error names a file, never its content. The token directory is a real
//! directory, not a symlink, owned by this process and writable by its owner only.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use arca_api::Scope;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::fresh::process_uid;

const PREFIX: &str = "arca_";
const BODY_LEN: usize = 43;
const SECRET_BYTES: usize = 32;
/// The largest hash file the server reads. A well-formed one is about 100 bytes.
const MAX_FILE_BYTES: u64 = 4096;
/// The longest id a hash file may name.
const MAX_ID_LEN: usize = 64;

/// The identity a request authenticated as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub token_id: String,
    pub scope: Scope,
    /// The token's optional tool allowlist. `None` admits every verb the scope does.
    pub tools: Option<Vec<String>>,
}

impl Caller {
    /// Whether this token's allowlist admits `tool`. A token without a list admits all of them.
    pub fn allows_tool(&self, tool: &str) -> bool {
        self.tools
            .as_ref()
            .is_none_or(|tools| tools.iter().any(|name| name == tool))
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

/// One loaded token: its id, its scope, its optional tool allowlist and the digest of its secret.
#[derive(Clone)]
struct Entry {
    id: String,
    scope: Scope,
    tools: Option<Vec<String>>,
    digest: [u8; 32],
}

/// The loaded token digests, read once at start and replaceable on SIGHUP. Cloning shares the live set.
#[derive(Clone)]
pub struct TokenStore {
    entries: Arc<RwLock<Vec<Entry>>>,
}

impl std::fmt::Debug for TokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenStore")
            .field(
                "tokens",
                &self.entries.read().map(|e| e.len()).unwrap_or_default(),
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
        for path in paths {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let id = name
                .strip_suffix(".hash")
                .filter(|id| valid_id(id))
                .ok_or_else(|| TokenStoreError::Malformed { file: name.clone() })?
                .to_string();
            let entry = load_one(&path, &name, id, owner_uid)?;
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
        Ok(TokenStore {
            entries: Arc::new(RwLock::new(entries)),
        })
    }

    /// Replaces the live token set from `dir` and returns its size. The previous set stays in place when the load fails.
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
        *self.entries.write().map_err(|_| TokenStoreError::Io {
            file: "the token store".into(),
        })? = next;
        Ok(len)
    }

    /// The caller a bearer authenticates as, or `None`. Every stored digest is compared, with no early exit.
    pub fn lookup(&self, bearer: &str) -> Option<Caller> {
        if !well_formed(bearer) {
            return None;
        }
        let digest = sha256(bearer.as_bytes());
        let entries = self.entries.read().ok()?;
        let mut found = None;
        for entry in entries.iter() {
            if digests_equal(&entry.digest, &digest) {
                found = Some(entry);
            }
        }
        found.map(|e| Caller {
            token_id: e.id.clone(),
            scope: e.scope,
            tools: e.tools.clone(),
        })
    }

    /// Every token's id, scope and optional tool list, by id. Never a digest or a secret.
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

/// A new secret of `scope`, and the text of its hash file.
pub fn mint(scope: Scope) -> (String, String) {
    mint_with_tools(scope, None)
}

/// A new secret of `scope` and its hash-file text, with an optional tool allowlist as the third line.
pub fn mint_with_tools(scope: Scope, tools: Option<&[String]>) -> (String, String) {
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

/// One hash file, checked before it is read: a regular file (not a symlink, not one swapped for a symlink between the
/// checks), mode exactly 0400, owned by `owner_uid`, at most `MAX_FILE_BYTES`, and well formed.
fn load_one(path: &Path, name: &str, id: String, owner_uid: u32) -> Result<Entry, TokenStoreError> {
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
    let (scope, tools, digest) = parse_body(text).ok_or_else(malformed)?;
    Ok(Entry {
        id,
        scope,
        tools,
        digest,
    })
}

/// The two or three lines of a hash file. Exactly these, LF endings, a final newline, and nothing after the digest. A
/// two-line file means every verb the scope admits; a third `tools=` line is the optional allowlist.
fn parse_body(text: &str) -> Option<(Scope, Option<Vec<String>>, [u8; 32])> {
    let body = text.strip_suffix('\n')?;
    let mut lines = body.split('\n');
    let scope = parse_scope(lines.next()?.strip_prefix("scope=")?)?;
    let digest = decode_hex(lines.next()?.strip_prefix("sha256=")?)?;
    let tools = match lines.next() {
        None => None,
        Some(line) => {
            let list = line.strip_prefix("tools=")?;
            Some(
                list.split(',')
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .collect(),
            )
        }
    };
    if lines.next().is_some() {
        return None;
    }
    Some((scope, tools, digest))
}

fn parse_scope(text: &str) -> Option<Scope> {
    match text {
        "viewer" => Some(Scope::Viewer),
        "doctor" => Some(Scope::Doctor),
        "operator" => Some(Scope::Operator),
        _ => None,
    }
}

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
