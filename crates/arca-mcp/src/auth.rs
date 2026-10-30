//! Interim bearer tokens: stored digests, a strict file format and one check per request.
//!
//! A token is the secret `arca_` followed by 43 base64url characters (32 random bytes). The server holds only the SHA-256
//! digest of the whole secret, in `<id>.hash`, whose text is `scope=<scope>` and `sha256=<64 lowercase hex>`, each on its
//! own line. A secret carries 256 random bits, so a plain digest is enough: a copied hash file yields no usable secret.
//! A secret is never stored, logged or echoed, and an error names a file, never its content.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use arca_api::Scope;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

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
}

/// Why a token directory will not load. Each variant names one file, never the file's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenStoreError {
    NotDir,
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

/// One loaded token: its id, its scope and the digest of its secret.
struct Entry {
    id: String,
    scope: Scope,
    digest: [u8; 32],
}

/// The loaded token digests, read once at start.
pub struct TokenStore {
    entries: Vec<Entry>,
}

impl std::fmt::Debug for TokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenStore")
            .field("tokens", &self.entries.len())
            .finish()
    }
}

impl TokenStore {
    /// Loads every hash file of `dir`, which must be owned by this process's effective uid.
    pub fn load(dir: &Path) -> Result<TokenStore, TokenStoreError> {
        let uid = effective_uid().map_err(|_| TokenStoreError::Io {
            file: "/proc/self/status".into(),
        })?;
        TokenStore::load_as(dir, uid)
    }

    /// Loads every hash file of `dir`, which must be owned by `owner_uid`. [`TokenStore::load`] passes this process's uid.
    /// A caller other than `load` names the uid the files must carry, so a test can check the owner refusal without root.
    pub fn load_as(dir: &Path, owner_uid: u32) -> Result<TokenStore, TokenStoreError> {
        if !fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
            return Err(TokenStoreError::NotDir);
        }
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
        Ok(TokenStore { entries })
    }

    /// The caller a bearer authenticates as, or `None`. Every stored digest is compared, with no early exit.
    pub fn lookup(&self, bearer: &str) -> Option<Caller> {
        if !well_formed(bearer) {
            return None;
        }
        let digest = sha256(bearer.as_bytes());
        let mut found = None;
        for entry in &self.entries {
            if digests_equal(&entry.digest, &digest) {
                found = Some(entry);
            }
        }
        found.map(|e| Caller {
            token_id: e.id.clone(),
            scope: e.scope,
        })
    }

    /// Every token's id and scope, by id. Never a digest or a secret.
    pub fn ids(&self) -> Vec<(String, Scope)> {
        self.entries
            .iter()
            .map(|e| (e.id.clone(), e.scope))
            .collect()
    }
}

/// A new secret of `scope`, and the text of its hash file.
pub fn mint(scope: Scope) -> (String, String) {
    let mut raw = [0u8; SECRET_BYTES];
    getrandom::fill(&mut raw).expect("the operating system's random source is available");
    let secret = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(raw));
    let text = format!(
        "scope={}\nsha256={}\n",
        scope.as_str(),
        to_hex(&sha256(secret.as_bytes()))
    );
    (secret, text)
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
    let (scope, digest) = parse_body(text).ok_or_else(malformed)?;
    Ok(Entry { id, scope, digest })
}

/// The two lines of a hash file. Exactly these, LF endings, a final newline, and nothing after the digest.
fn parse_body(text: &str) -> Option<(Scope, [u8; 32])> {
    let body = text.strip_suffix('\n')?;
    let mut lines = body.split('\n');
    let scope = parse_scope(lines.next()?.strip_prefix("scope=")?)?;
    let digest = decode_hex(lines.next()?.strip_prefix("sha256=")?)?;
    if lines.next().is_some() {
        return None;
    }
    Some((scope, digest))
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

/// The effective uid of this process, from `/proc/self/status`, the second field of its `Uid:` line.
fn effective_uid() -> io::Result<u32> {
    let status = fs::read_to_string("/proc/self/status")?;
    let line = status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .ok_or_else(|| io::Error::other("no Uid line"))?;
    line.split_whitespace()
        .nth(1)
        .and_then(|field| field.parse().ok())
        .ok_or_else(|| io::Error::other("no effective uid"))
}
