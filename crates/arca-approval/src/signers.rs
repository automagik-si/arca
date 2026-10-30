//! The owner's signers file: the trust root that says which hardware keys may sign an Arca approval.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ssh_key::{HashAlg, PublicKey};

use crate::{ARCA_NAMESPACE, ApprovalRefusal};

/// The owner's signers file in production. Root-owned, and the only file a production kernel trusts.
pub const OWNER_SIGNERS: &str = "/etc/arca/owner-signers";
/// The prefix every project of a scratch kernel carries, and the only projects the test injection may serve.
pub const SCRATCH_PREFIX: &str = "arcak-scratch-";
/// Whether this build honours the scratch-only test injection at all. Only a build with debug assertions does. A
/// release build can turn debug assertions on through its profile or `-C debug-assertions`, and then it honours the
/// injection too. The scratch check in `signers_trust` still applies in that build, so the injection never serves a
/// production kernel.
pub const TEST_INJECTION_BUILT: bool = cfg!(debug_assertions);

/// The largest signers file that is read, in bytes.
const SIGNERS_MAX_BYTES: u64 = 64 * 1024;
/// The principal the owner's key lines name, as the owner's template does.
const PRINCIPAL: &str = "owner";
/// The one key type kept: a FIDO key whose signature carries the user-presence flag.
const HARDWARE_KEY_TYPE: &str = "sk-ssh-ed25519@openssh.com";
/// The group- and other-write bits. A file or directory with either set is not trusted.
const WRITABLE_BY_OTHERS: u32 = 0o022;

/// Who must own the signers file and each directory above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustRoot {
    /// Root (uid 0) owns the file and every directory above it, up to `/`.
    Root,
    /// The uid owns the file and every directory above it, up to and including `up_to`. A test's own temporary directory.
    Scratch { uid: u32, up_to: PathBuf },
}

/// A key line the signers file holds for Arca's namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerLine {
    /// The key's type, `sk-ssh-ed25519@openssh.com`.
    pub key_type: String,
    /// The key's SSH public key blob.
    pub public: Vec<u8>,
    /// `SHA256:<base64>` of the blob, as `ssh-keygen` prints it.
    pub fingerprint: String,
}

/// The signers file to read and the trust it must carry.
///
/// `test_signers` is the scratch-only injection (`ARCA_TEST_OWNER_SIGNERS` in the binary). It is honoured only when
/// `scratch` is true, every project starts with [`SCRATCH_PREFIX`] (and there is at least one), and this build honours
/// the injection. Otherwise it refuses start, so no production kernel trusts a file its own user could write.
pub fn signers_trust(
    scratch: bool,
    projects: &[String],
    test_signers: Option<PathBuf>,
) -> Result<(PathBuf, TrustRoot), ApprovalRefusal> {
    let Some(path) = test_signers else {
        return Ok((PathBuf::from(OWNER_SIGNERS), TrustRoot::Root));
    };
    if !TEST_INJECTION_BUILT {
        return Err(untrusted(
            "a test signers file is named, and this build does not honour one".to_string(),
        ));
    }
    let scratch_only = scratch
        && !projects.is_empty()
        && projects
            .iter()
            .all(|p| p.len() > SCRATCH_PREFIX.len() && p.starts_with(SCRATCH_PREFIX));
    if !scratch_only {
        return Err(untrusted(format!(
            "a test signers file is named, and the kernel is not a scratch kernel whose every project starts with {SCRATCH_PREFIX}"
        )));
    }
    if !path.is_absolute() {
        return Err(untrusted(
            "the test signers file is not an absolute path".to_string(),
        ));
    }
    let Some(up_to) = path.parent() else {
        return Err(untrusted(
            "the test signers file has no directory above it".to_string(),
        ));
    };
    Ok((
        path.clone(),
        TrustRoot::Scratch {
            uid: effective_uid()?,
            up_to: up_to.to_path_buf(),
        },
    ))
}

/// The signers file's key lines for Arca's namespace, read under `trust`.
///
/// Keeps only `sk-ssh-ed25519@openssh.com` lines whose `namespaces=` list names [`ARCA_NAMESPACE`]. Any other line of the
/// owner's template shape (another namespace, another key type) is skipped. A line that is not of that shape refuses the
/// whole file, so the file never means more than its lines say.
pub fn load_signers(path: &Path, trust: &TrustRoot) -> Result<Vec<SignerLine>, ApprovalRefusal> {
    let bytes = read_trusted(path, trust)?;
    parse_signers(&bytes)
}

/// The uid that runs this process, from its status. Used to name the owner of a scratch test's files.
pub(crate) fn effective_uid() -> Result<u32, ApprovalRefusal> {
    let unreadable = || untrusted("the effective uid cannot be read".to_string());
    let status = fs::read_to_string("/proc/self/status").map_err(|_| unreadable())?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().nth(1))
        .and_then(|effective| effective.parse().ok())
        .ok_or_else(unreadable)
}

fn untrusted(why: String) -> ApprovalRefusal {
    ApprovalRefusal::Untrusted(why)
}

/// The owner uid, and the uid of every directory above the file, that `trust` names, plus the directory it stops at.
fn trust_parts(trust: &TrustRoot) -> (u32, Option<&Path>) {
    match trust {
        TrustRoot::Root => (0, None),
        TrustRoot::Scratch { uid, up_to } => (*uid, Some(up_to.as_path())),
    }
}

/// The file's bytes, read only when the file and every directory above it carry `trust`: one that is not a symbolic link,
/// owned by the trusted uid, and not group- or other-writable.
fn read_trusted(path: &Path, trust: &TrustRoot) -> Result<Vec<u8>, ApprovalRefusal> {
    let (uid, up_to) = trust_parts(trust);
    if !path.is_absolute() {
        return Err(untrusted("the signers path is not absolute".to_string()));
    }
    // The directories checked are the path's own ancestors, so a `.` or `..` would let the walk and the open disagree.
    if path
        .components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return Err(untrusted(
            "the signers path has a `.` or `..` component".to_string(),
        ));
    }
    check_directories(path, uid, up_to)?;

    let before = fs::symlink_metadata(path).map_err(|e| {
        untrusted(format!(
            "the signers file {} cannot be read ({})",
            path.display(),
            e.kind()
        ))
    })?;
    if before.file_type().is_symlink() {
        return Err(untrusted(format!(
            "the signers file {} is a symbolic link",
            path.display()
        )));
    }
    if !before.is_file() {
        return Err(untrusted(format!(
            "the signers file {} is not a regular file",
            path.display()
        )));
    }
    check_file(&before, uid, path)?;

    let mut file = File::open(path).map_err(|e| {
        untrusted(format!(
            "the signers file {} cannot be opened ({})",
            path.display(),
            e.kind()
        ))
    })?;
    let opened = file.metadata().map_err(|e| {
        untrusted(format!(
            "the signers file {} cannot be inspected ({})",
            path.display(),
            e.kind()
        ))
    })?;
    if (opened.dev(), opened.ino()) != (before.dev(), before.ino()) {
        return Err(untrusted(format!(
            "the signers file {} changed while it was opened",
            path.display()
        )));
    }
    check_file(&opened, uid, path)?;

    let mut bytes = Vec::new();
    (&mut file)
        .take(SIGNERS_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| {
            untrusted(format!(
                "the signers file {} cannot be read ({})",
                path.display(),
                e.kind()
            ))
        })?;
    if bytes.len() as u64 > SIGNERS_MAX_BYTES {
        return Err(untrusted(format!(
            "the signers file {} is larger than 64 KiB",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Every directory above `path`, up to `up_to` when one is named, else up to `/`: a real directory, owned by `uid`, and
/// not group- or other-writable.
fn check_directories(path: &Path, uid: u32, up_to: Option<&Path>) -> Result<(), ApprovalRefusal> {
    if let Some(stop) = up_to
        && !path.ancestors().any(|ancestor| ancestor == stop)
    {
        return Err(untrusted(format!(
            "{} is not above the signers file",
            stop.display()
        )));
    }
    for dir in path.ancestors().skip(1) {
        let meta = fs::symlink_metadata(dir)
            .map_err(|e| untrusted(format!("{} cannot be read ({})", dir.display(), e.kind())))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(untrusted(format!(
                "{} is not a directory, or is a symbolic link",
                dir.display()
            )));
        }
        if meta.uid() != uid {
            return Err(untrusted(format!(
                "{} is not owned by the trusted uid",
                dir.display()
            )));
        }
        if meta.mode() & WRITABLE_BY_OTHERS != 0 {
            return Err(untrusted(format!(
                "{} is group- or other-writable",
                dir.display()
            )));
        }
        if up_to == Some(dir) {
            break;
        }
    }
    Ok(())
}

fn check_file(meta: &fs::Metadata, uid: u32, path: &Path) -> Result<(), ApprovalRefusal> {
    if meta.uid() != uid {
        return Err(untrusted(format!(
            "the signers file {} is not owned by the trusted uid",
            path.display()
        )));
    }
    if meta.mode() & WRITABLE_BY_OTHERS != 0 {
        return Err(untrusted(format!(
            "the signers file {} is group- or other-writable",
            path.display()
        )));
    }
    Ok(())
}

/// The key lines of a signers file, strictly. A control character anywhere refuses. A blank line or a comment is skipped.
/// Any other line must be `owner namespaces="<list>" <key type> <base64> [comment]`, with single spaces, or the file
/// refuses by its line number. Only a hardware key listing the Arca namespace is kept.
pub(crate) fn parse_signers(bytes: &[u8]) -> Result<Vec<SignerLine>, ApprovalRefusal> {
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    let mut keys = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let number = index + 1;
        if raw.iter().any(|b| (*b < 0x20 && *b != b'\t') || *b == 0x7f) {
            return Err(untrusted(format!(
                "line {number} of the signers file carries a control character"
            )));
        }
        let body = raw.trim_ascii_start();
        if body.is_empty() || body[0] == b'#' {
            continue;
        }
        if !raw.is_ascii() {
            return Err(untrusted(format!(
                "line {number} of the signers file carries a non-ASCII byte"
            )));
        }
        let text = std::str::from_utf8(raw)
            .map_err(|_| untrusted(format!("line {number} of the signers file is not text")))?;
        let Some((namespaces, key_type, encoded)) = parse_key_line(text) else {
            return Err(untrusted(format!(
                "line {number} of the signers file is not blank, a comment or an owner key line"
            )));
        };
        if !namespaces.split(',').any(|ns| ns == ARCA_NAMESPACE) || key_type != HARDWARE_KEY_TYPE {
            continue;
        }
        let blob = STANDARD
            .decode(encoded)
            .map_err(|_| untrusted(format!("line {number} of the signers file holds no key")))?;
        let key = PublicKey::from_bytes(&blob)
            .map_err(|_| untrusted(format!("line {number} of the signers file holds no key")))?;
        if !key.key_data().is_sk_ed25519() || key.algorithm().as_str() != key_type {
            return Err(untrusted(format!(
                "line {number} of the signers file holds a key that is not a {HARDWARE_KEY_TYPE} key"
            )));
        }
        keys.push(SignerLine {
            key_type: key_type.to_string(),
            public: blob,
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        });
    }
    Ok(keys)
}

/// `owner namespaces="<list>" <key type> <base64> [comment]` as its three parts, or `None` for any other shape. The list
/// holds lowercase names, digits, dots, dashes and commas. The key type and the base64 hold no space, and the comment is
/// printable ASCII.
fn parse_key_line(text: &str) -> Option<(&str, &str, &str)> {
    let rest = text
        .strip_prefix(PRINCIPAL)?
        .strip_prefix(" namespaces=\"")?;
    let (namespaces, rest) = rest.split_once('"')?;
    let rest = rest.strip_prefix(' ')?;
    let mut parts = rest.splitn(3, ' ');
    let key_type = parts.next()?;
    let encoded = parts.next()?;
    let comment = parts.next().unwrap_or("");
    let names_ok = !namespaces.is_empty()
        && namespaces.split(',').all(|ns| {
            !ns.is_empty()
                && ns
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        });
    let type_ok = !key_type.is_empty()
        && key_type
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'.' | b'-'));
    let body = encoded.trim_end_matches('=');
    let padding = encoded.len() - body.len();
    let encoded_ok = !body.is_empty()
        && padding <= 2
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/');
    let comment_ok = comment.bytes().all(|b| (0x20..=0x7e).contains(&b));
    (names_ok && type_ok && encoded_ok && comment_ok).then_some((namespaces, key_type, encoded))
}
