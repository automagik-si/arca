//! Shared test helpers. Every owner key is a software `sk-ssh-ed25519` key from a fixed seed, and it reaches the verifier only
//! through a signers file the test writes into its own scratch directory and reads back through `signers_trust` and
//! `load_signers`. No hardware key is used, no owner file is read, and no helper prints a signature or an approval string.
#![allow(dead_code)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use arca_approval::{ApprovalVerifier, FileNonces, SshSigVerifier, load_signers, signers_trust};
use arca_incus::{FakeIncus, FakeState, IncusRead, UnixIncus};
use arca_kernel::canon::{Digest, canonical_json, params_digest};
use arca_kernel::desired::{
    DesiredStateGate, Document, Entry, EntryId, PluginId, SignedDocument, sign_document,
};
use arca_kernel::identity::NodeKey;
use arca_kernel::journal::{FileJournal, Journal};
use arca_providers::incus::{Family, IncusProvider, RetainedSet};
use arca_reconcile::toy::Toy;
use arca_reconcile::{PluginSet, Reconciler};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256, Sha512};

pub const SK_TYPE: &str = "sk-ssh-ed25519@openssh.com";
/// The approval namespace the verifier checks (`arca_approval::ARCA_NAMESPACE`).
pub const NAMESPACE: &str = "arca-kernel-approve";
/// The host the approvals name. The verifier is built for it.
pub const HOST: &str = "node1";
/// The scratch project the toy and every test family use.
pub const PROJECT: &str = "arcak-scratch-toy";
/// The fixture's reserved droidd project, which no family may name.
pub const DROIDD_PROJECT: &str = "droid";
/// 2026-10-30T12:00:00.000Z in epoch milliseconds: the clock every test runs at.
pub const NOW: u64 = 1_793_361_600_000;
/// The kernel's entry mark.
pub const MARK: &str = "user.arcakernel.entry";
/// The fixture every fake-backed test starts from.
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arca-incus/tests/fixtures/state.json"
);

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh directory under the default temporary directory, mode 0700, removed on drop.
pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "arca-reconcile-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir(&dir).expect("the scratch directory is created");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
        Self { dir }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// A software `sk-ssh-ed25519@openssh.com` key from a fixed seed.
pub struct SkKey {
    signing: SigningKey,
}

impl SkKey {
    pub fn from_seed(seed: u8) -> Self {
        Self {
            signing: SigningKey::from_bytes(&[seed; 32]),
        }
    }

    fn blob(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, SK_TYPE.as_bytes());
        put(&mut out, &self.signing.verifying_key().to_bytes());
        put(&mut out, b"ssh:");
        out
    }

    /// The signers-file line for this key, listing `namespaces`.
    pub fn line(&self, namespaces: &str) -> String {
        format!(
            "owner namespaces=\"{namespaces}\" {SK_TYPE} {} test-key",
            STANDARD.encode(self.blob())
        )
    }

    /// An armoured SSHSIG signature of `message` in `namespace`, made with the user-presence flag set.
    pub fn sign(&self, namespace: &str, message: &[u8]) -> String {
        let data = signed_data(namespace, message);
        let mut inner = Vec::new();
        inner.extend_from_slice(&Sha256::digest(b"ssh:"));
        inner.push(0x01);
        let counter: u32 = 7;
        inner.extend_from_slice(&counter.to_be_bytes());
        inner.extend_from_slice(&Sha256::digest(&data));
        let ed = self.signing.sign(&inner).to_bytes();
        let mut sig = Vec::new();
        put(&mut sig, SK_TYPE.as_bytes());
        put(&mut sig, &ed);
        sig.push(0x01);
        sig.extend_from_slice(&counter.to_be_bytes());
        armour(&sshsig(&self.blob(), namespace, &sig))
    }
}

/// The data an SSHSIG signature covers.
fn signed_data(namespace: &str, message: &[u8]) -> Vec<u8> {
    let mut out = b"SSHSIG".to_vec();
    put(&mut out, namespace.as_bytes());
    put(&mut out, b"");
    put(&mut out, b"sha512");
    put(&mut out, &Sha512::digest(message));
    out
}

fn sshsig(public: &[u8], namespace: &str, signature: &[u8]) -> Vec<u8> {
    let mut out = b"SSHSIG".to_vec();
    out.extend_from_slice(&1u32.to_be_bytes());
    put(&mut out, public);
    put(&mut out, namespace.as_bytes());
    put(&mut out, b"");
    put(&mut out, b"sha512");
    put(&mut out, signature);
    out
}

fn armour(blob: &[u8]) -> String {
    let body = STANDARD.encode(blob);
    let mut out = String::from("-----BEGIN SSH SIGNATURE-----\n");
    for chunk in body.as_bytes().chunks(70) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push('\n');
    }
    out.push_str("-----END SSH SIGNATURE-----\n");
    out
}

/// The approval string for a statement: `<base64 statement>.<base64 armoured signature>`.
pub fn approval_of(statement: &str, armoured: &str) -> String {
    format!(
        "{}.{}",
        STANDARD.encode(statement),
        STANDARD.encode(armoured)
    )
}

/// The time `offset_ms` after `NOW`, in `toISOString` form. Every offset used stays inside the same hour.
pub fn at(offset_ms: i64) -> String {
    let total = 12 * 3_600_000 + offset_ms;
    format!(
        "2026-10-30T{:02}:{:02}:{:02}.{:03}Z",
        total / 3_600_000,
        total % 3_600_000 / 60_000,
        total % 60_000 / 1000,
        total % 1000
    )
}

/// The target an approval of plan `plan` names.
pub fn plan_target(plan: &Digest) -> String {
    format!("plan-{}", plan.to_hex())
}

/// The arguments an approval of plan `plan` covers.
pub fn plan_args(plan: &Digest) -> Value {
    json!({ "plan": plan.to_hex() })
}

/// An approval of `args` for `target`, signed by `key`, issued at `NOW` and valid for ten minutes. `nonce` makes it unique.
pub fn approval(key: &SkKey, target: &str, args: &Value, nonce: &str) -> String {
    let statement = json!({
        "host": HOST,
        "action": "delete",
        "target": target,
        "paramsDigest": params_digest(args).expect("the arguments are an object"),
        "issuedAt": at(0),
        "expiresAt": at(10 * 60_000),
        "nonce": nonce,
    });
    let text = canonical_json(&statement).expect("the statement canonicalizes");
    approval_of(&text, &key.sign(NAMESPACE, text.as_bytes()))
}

/// An approval of `args` for `target`, signed by `key`, whose window runs from `issued_ms` to `expires_ms` after `NOW`.
pub fn approval_window(
    key: &SkKey,
    target: &str,
    args: &Value,
    nonce: &str,
    issued_ms: i64,
    expires_ms: i64,
) -> String {
    let statement = json!({
        "host": HOST,
        "action": "delete",
        "target": target,
        "paramsDigest": params_digest(args).expect("the arguments are an object"),
        "issuedAt": at(issued_ms),
        "expiresAt": at(expires_ms),
        "nonce": nonce,
    });
    let text = canonical_json(&statement).expect("the statement canonicalizes");
    approval_of(&text, &key.sign(NAMESPACE, text.as_bytes()))
}

/// An approval of plan `plan`, signed by `key`, with a nonce derived from `tag`.
pub fn approval_for_plan(key: &SkKey, plan: &Digest, tag: u32) -> String {
    approval(key, &plan_target(plan), &plan_args(plan), &nonce(tag))
}

/// A 32-character lowercase hex nonce derived from `tag`.
pub fn nonce(tag: u32) -> String {
    format!(
        "{:032x}",
        0xabc0_0000_0000_0000_0000_0000_0000_0000u128 | tag as u128
    )
}

/// A verifier that trusts `key` alone, built the way the kernel builds one: the signers file is written into `scratch`,
/// read back through `signers_trust` and `load_signers`, and the nonce file is opened in `scratch`.
pub fn verifier_for(
    scratch: &Scratch,
    key: &SkKey,
    tag: &str,
) -> ApprovalVerifier<SshSigVerifier, FileNonces> {
    let file = scratch.path(&format!("owner-signers-{tag}"));
    fs::write(&file, format!("{}\n", key.line(NAMESPACE))).expect("the signers file is written");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("mode 0600");
    let (path, trust) = signers_trust(true, &[PROJECT.to_string()], Some(file))
        .expect("the test signers file is named in a scratch kernel");
    let lines = load_signers(&path, &trust).expect("the signers file loads");
    let nonces = FileNonces::open(&scratch.path(&format!("nonces-{tag}.jsonl")))
        .expect("the nonce file opens");
    ApprovalVerifier::new(HOST.to_string(), SshSigVerifier::new(lines), nonces)
}

/// A node key, generated fresh for one test.
pub fn node_key() -> NodeKey {
    NodeKey::generate()
}

/// A signed document for `key`'s node with `entries`, valid from `NOW` to one hour later.
pub fn signed(
    key: &NodeKey,
    seq: u64,
    prev: Option<Digest>,
    entries: Vec<Entry>,
) -> SignedDocument {
    sign_document(
        Document {
            node: key.id(),
            seq,
            prev,
            issued_at: at(0),
            expires_at: at(60 * 60_000),
            entries,
        },
        key,
    )
}

/// A desired-state entry for `id`, of plugin `plugin`, with `config`.
pub fn entry(id: &str, plugin: &str, config: Value) -> Entry {
    let text = canonical_json(&config).expect("the config canonicalizes");
    Entry {
        id: EntryId(id.to_string()),
        plugin: PluginId(plugin.to_string()),
        version: "0.1.0".to_string(),
        digest: Digest::of(text.as_bytes()),
        signer: "node".to_string(),
        config,
        disabled: false,
    }
}

/// The toy entry the removal suite applies, with its tag.
pub fn toy_entry(tag: &str) -> Entry {
    entry("toy-scratch", "toy.scratch", json!({ "tag": tag }))
}

/// The family every test provider writes in: the scratch project, and the droidd project it must never touch.
pub fn family() -> Family {
    Family {
        projects: vec![PROJECT.to_string()],
        scratch: true,
        droidd_projects: vec![DROIDD_PROJECT.to_string()],
        reserved_cidrs: vec!["198.51.100.0/24".to_string()],
    }
}

/// The fixture state, with the scratch project added. Its features let an ACL and an instance be created.
pub fn fake_state() -> FakeState {
    let mut state = FakeState::load(Path::new(FIXTURE)).expect("the fixture loads");
    state.projects.push(json!({
        "name": PROJECT,
        "description": "",
        "config": {
            "features.networks": "true",
            "features.networks.zones": "true",
            "features.profiles": "true",
            "features.storage.volumes": "true",
            "features.storage.buckets": "true",
        },
    }));
    state
}

/// The fake Incus, served from a fresh scratch directory.
pub fn start_fake(scratch: &Scratch) -> FakeIncus {
    FakeIncus::start(&scratch.dir, fake_state()).expect("the fake serves")
}

/// The Incus provider over `fake`, sharing `retained`.
pub fn provider(scratch: &Scratch, fake: &FakeIncus, retained: &RetainedSet) -> IncusProvider {
    IncusProvider::new(
        fake.socket().to_path_buf(),
        family(),
        scratch.path("secrets"),
        retained.clone(),
    )
    .expect("the family is valid")
}

/// A GET of `path` on the fake, or `None` when Incus answers that the object is not there.
pub fn get(fake: &FakeIncus, path: &str) -> Option<Value> {
    UnixIncus::new(fake.socket().to_path_buf()).get(path).ok()
}

/// A raw request on the fake's socket, for a change the test makes outside the provider. Returns the status and the body.
pub fn raw(fake: &FakeIncus, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let mut stream = UnixStream::connect(fake.socket()).expect("the fake accepts");
    let payload = body.map(|value| value.to_string()).unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: incus\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream
        .write_all(head.as_bytes())
        .expect("the head is written");
    stream
        .write_all(payload.as_bytes())
        .expect("the body is written");
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).expect("the answer is read");
    let text = String::from_utf8_lossy(&answer).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .expect("the answer has a status");
    let body_text = text.split("\r\n\r\n").nth(1).unwrap_or("");
    (
        status,
        serde_json::from_str(body_text).unwrap_or(Value::Null),
    )
}

/// The digest of an object's own config, as the watch and the ownership tests take it.
pub fn config_hash(fake: &FakeIncus, path: &str) -> Option<String> {
    let object = get(fake, path)?;
    let config = object.get("config").cloned().unwrap_or_else(|| json!({}));
    let text = canonical_json(&config).ok()?;
    Some(Digest::of(text.as_bytes()).to_hex())
}

/// The name of every instance, across projects, whose own config carries `mark` with the value `value`. The fixture holds marks of
/// its own entries, so a mark is matched by value, not by key alone.
pub fn instances_marked(fake: &FakeIncus, mark: &str, value: &str) -> Vec<String> {
    let listing = get(fake, "/1.0/instances?all-projects=true&recursion=1").unwrap_or(json!([]));
    listing
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["config"][mark].as_str() == Some(value))
        .filter_map(|row| row["name"].as_str().map(str::to_string))
        .collect()
}

/// How many DELETE requests the fake received for an object whose path contains `fragment`.
pub fn deletes_for(fake: &FakeIncus, fragment: &str) -> usize {
    fake.writes()
        .iter()
        .filter(|write| write.method == "DELETE" && write.path.contains(fragment))
        .count()
}

/// The reconciler the suite runs: a durable journal in `scratch`, the toy plugin, and `retained` shared with the provider.
pub fn reconciler(
    scratch: &Scratch,
    fake: &FakeIncus,
    key: &NodeKey,
    retained: &RetainedSet,
) -> Reconciler<FileJournal> {
    let journal = Journal::open(
        FileJournal::open(&scratch.path("journal.jsonl")).expect("the journal opens"),
    )
    .expect("the journal folds");
    let mut plugins = PluginSet::new();
    plugins.add::<Toy>().expect("the toy plugin is valid");
    Reconciler::open(
        DesiredStateGate::new(key.public(), None),
        journal,
        plugins,
        Box::new(provider(scratch, fake, retained)),
        retained.clone(),
    )
    .expect("the journal has no unresolved intent")
}

/// The path of an instance in `project`.
pub fn instance_path(name: &str) -> String {
    format!("/1.0/instances/{name}?project={PROJECT}")
}

/// The path of an ACL in `PROJECT`.
pub fn acl_path(name: &str) -> String {
    format!("/1.0/network-acls/{name}?project={PROJECT}")
}
