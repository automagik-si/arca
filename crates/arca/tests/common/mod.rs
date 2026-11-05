//! Shared test helpers. Every owner key is a software `sk-ssh-ed25519` key from a fixed seed, and it reaches the verifier only
//! through a signers file the test writes into its own scratch directory and reads back through `signers_trust` and
//! `load_signers`. No hardware key is used, no owner file is read, and no helper prints a signature or an approval string.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use arca::handlers::{Clock, plan_handlers};
use arca::kernel::Kernel;
use arca_approval::{
    ApprovalVerifier, FileNonces, FileSignCounts, OwnerKeys, PASSKEY_COUNTS_FILE, PasskeyVerifier,
    SshSigVerifier, load_passkeys, load_signers, passkeys_trust, signers_trust,
};
use arca_incus::{FakeIncus, FakeState, IncusRead, UnixIncus};
use arca_kernel::canon::{Digest, canonical_json, params_digest};
use arca_kernel::desired::{Entry, EntryId, PluginId};
use arca_kernel::identity::NodeKey;
use arca_kernel::plugin::Provider;
use arca_mcp::{CallError, Caller, ToolHandler};
use arca_providers::incus::{Family, IncusProvider, RetainedSet};
use arca_reconcile::PluginSet;
use arca_reconcile::toy::Toy;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha512};

pub const SK_TYPE: &str = "sk-ssh-ed25519@openssh.com";
/// The approval namespace the verifier checks (`arca_approval::ARCA_NAMESPACE`).
pub const NAMESPACE: &str = "arca-kernel-approve";
/// The host the node names in every statement and every verifier.
pub const HOST: &str = "node1";
/// The scratch project the toy and every test family use.
pub const PROJECT: &str = "arcak-scratch-toy";
/// The kernel's entry mark.
pub const MARK: &str = "user.arcakernel.entry";
/// 2026-10-30T12:00:00.000Z in epoch milliseconds: the clock every test runs at.
pub const NOW: u64 = 1_793_361_600_000;
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
            "arca-plans-{tag}-{}-{nanos}-{seq}",
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
        inner.extend_from_slice(&sha2::Sha256::digest(b"ssh:"));
        inner.push(0x01);
        let counter: u32 = 7;
        inner.extend_from_slice(&counter.to_be_bytes());
        inner.extend_from_slice(&sha2::Sha256::digest(&data));
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

/// The approval string for a statement's bytes and an armoured signature: `<base64 statement>.<base64 armoured signature>`.
pub fn approval_of(statement: &str, armoured: &str) -> String {
    format!(
        "{}.{}",
        STANDARD.encode(statement),
        STANDARD.encode(armoured)
    )
}

/// The approval string an owner key makes for `statement`, the exact bytes `plan_list` returned.
pub fn approve_statement(key: &SkKey, statement: &str) -> String {
    approval_of(statement, &key.sign(NAMESPACE, statement.as_bytes()))
}

/// A verifier that trusts the SSH `keys` alone, built the way the kernel builds one: the signers file is written into
/// `scratch`, read back through `signers_trust` and `load_signers`, and the nonce file is opened in `scratch`. No passkey is
/// enrolled. An empty `keys` is a node whose owner has installed no key.
pub fn verifier_for(
    scratch: &Scratch,
    keys: &[&SkKey],
    tag: &str,
) -> ApprovalVerifier<OwnerKeys, FileNonces> {
    verifier_with_passkeys(scratch, keys, "", tag)
}

/// As `verifier_for`, with the passkeys file `passkeys` written into `scratch` and read back through `passkeys_trust` and
/// `load_passkeys`, and the sign counts kept in `scratch`.
pub fn verifier_with_passkeys(
    scratch: &Scratch,
    keys: &[&SkKey],
    passkeys: &str,
    tag: &str,
) -> ApprovalVerifier<OwnerKeys, FileNonces> {
    let file = scratch.path(&format!("owner-signers-{tag}"));
    let text: String = keys
        .iter()
        .map(|key| format!("{}\n", key.line(NAMESPACE)))
        .collect();
    fs::write(&file, text).expect("the signers file is written");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("mode 0600");
    let (path, trust) = signers_trust(true, &[PROJECT.to_string()], Some(file))
        .expect("the test signers file is named in a scratch kernel");
    let lines = load_signers(&path, &trust).expect("the signers file loads");
    let nonces = FileNonces::open(&scratch.path(&format!("nonces-{tag}.jsonl")))
        .expect("the nonce file opens");
    let file = scratch.path(&format!("owner-passkeys-{tag}"));
    fs::write(&file, passkeys).expect("the passkeys file is written");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("mode 0600");
    let (path, trust) = passkeys_trust(true, &[PROJECT.to_string()], Some(file))
        .expect("the test passkeys file is named in a scratch kernel");
    let passkey_lines = load_passkeys(&path, &trust).expect("the passkeys file loads");
    let counts = FileSignCounts::open(&scratch.path(&format!("{tag}-{PASSKEY_COUNTS_FILE}")))
        .expect("the sign-count file opens");
    ApprovalVerifier::new(
        HOST.to_string(),
        OwnerKeys::new(
            SshSigVerifier::new(lines),
            PasskeyVerifier::new(passkey_lines, counts),
        ),
        nonces,
    )
}

/// The node key of the state directory `dir`, created on first use and loaded after that.
pub fn node_key_at(dir: &Path) -> NodeKey {
    NodeKey::load_or_create(&dir.join("node.key")).expect("the node key loads")
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

/// The toy entry with id `toy-scratch`, tagged `tag`: one ACL (class A) and one instance (class B).
pub fn toy_entry(tag: &str) -> Entry {
    entry("toy-scratch", "toy.scratch", json!({ "tag": tag }))
}

/// A second toy entry, with its own id, so two plans can wait at once.
pub fn second_toy_entry(tag: &str) -> Entry {
    entry("toy-second", "toy.scratch", json!({ "tag": tag }))
}

/// The family every test provider writes in.
pub fn family() -> Family {
    Family {
        projects: vec![PROJECT.to_string()],
        scratch: true,
        droidd_projects: vec!["droid".to_string()],
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

/// The fake Incus, served from `scratch`.
pub fn start_fake(scratch: &Scratch) -> FakeIncus {
    FakeIncus::start(&scratch.dir, fake_state()).expect("the fake serves")
}

/// The Incus write provider over `fake`, sharing `retained`. Its secrets live in the scratch directory.
pub fn provider(scratch: &Scratch, fake: &FakeIncus, retained: &RetainedSet) -> IncusProvider {
    IncusProvider::new(
        fake.socket().to_path_buf(),
        family(),
        scratch.path("secrets"),
        retained.clone(),
    )
    .expect("the family is valid")
}

/// A kernel over `scratch`'s state directory, with the toy plugin, the write provider over `fake`, the approval `verifier` and
/// the clock `now`. Opening it again over the same directory is a restart.
pub fn kernel(
    scratch: &Scratch,
    fake: &FakeIncus,
    verifier: ApprovalVerifier<OwnerKeys, FileNonces>,
    now: u64,
) -> Result<Kernel, arca_kernel::KernelError> {
    let mut plugins = PluginSet::new();
    plugins.add::<Toy>().expect("the toy plugin is valid");
    let socket = fake.socket().to_path_buf();
    let secrets = scratch.path("secrets");
    Kernel::open(
        &scratch.dir,
        node_key_at(&scratch.dir),
        plugins,
        verifier,
        HOST.to_string(),
        move |retained| {
            let provider = IncusProvider::new(socket, family(), secrets, retained)
                .map_err(|refusal| arca_kernel::KernelError::Plugin(refusal.to_string()))?;
            let boxed: Box<dyn Provider + Send> = Box::new(provider);
            Ok(boxed)
        },
        now,
    )
}

/// The plan verbs' handlers over `kernel`, keyed by verb name, with the clock `now` read at each call.
pub fn plan_verbs(
    kernel: std::sync::Arc<std::sync::Mutex<Kernel>>,
    now: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> std::collections::HashMap<&'static str, std::sync::Arc<dyn ToolHandler>> {
    let clock: Clock = std::sync::Arc::new(move || now.load(Ordering::SeqCst));
    plan_handlers(kernel, clock).into_iter().collect()
}

/// A caller of scope `scope`, for direct calls to a handler.
pub fn caller(scope: arca_api::Scope) -> Caller {
    Caller {
        token_id: "test-caller".to_string(),
        scope,
        tools: None,
    }
}

/// Calls the handler for `verb` with `args`, as an operator would.
pub fn call_verb(
    handlers: &std::collections::HashMap<&'static str, std::sync::Arc<dyn ToolHandler>>,
    verb: &str,
    args: Value,
) -> Result<Value, CallError> {
    handlers
        .get(verb)
        .expect("the verb has a handler")
        .call(args, &caller(arca_api::Scope::Operator))
}

/// A GET of `path` on the fake, or `None` when Incus answers that the object is not there.
pub fn get(fake: &FakeIncus, path: &str) -> Option<Value> {
    UnixIncus::new(fake.socket().to_path_buf()).get(path).ok()
}

/// The name of every instance, across projects, whose own config carries `mark` with the value `value`.
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

/// The digest of a plan's arguments, as the statement carries it.
pub fn plan_args_digest(plan_hex: &str) -> String {
    params_digest(&json!({ "plan": plan_hex })).expect("the arguments are an object")
}

// Adoption. The instance the adoption tests take is `svc1`: a running container in `default` whose NIC comes from the default
// profile, on the bridge `br1`, and whose root is its own device. Its MAC holds the lease of `ADOPT_IP`.

/// The adopted instance's MAC.
pub const ADOPT_MAC: &str = "10:66:6a:00:00:01";
/// The adopted instance's IPv4, which its MAC leases.
pub const ADOPT_IP: &str = "192.0.2.131";
/// The adopted instance's uuid.
pub const ADOPT_UUID: &str = "00000000-0000-4000-8000-0000000000a1";
/// The adoption project.
pub const ADOPT_TO: &str = "arcak-adopt-svc1";

/// The instance `svc1` in `project`, with `extra` added to its own config.
pub fn adopt_instance(project: &str, extra: Value) -> Value {
    let mut config = json!({ "volatile.uuid": ADOPT_UUID, "volatile.eth0.hwaddr": ADOPT_MAC, "image.os": "debian" });
    if let (Some(config), Value::Object(extra)) = (config.as_object_mut(), extra) {
        config.extend(extra);
    }
    json!({
        "name": "svc1", "project": project, "type": "container", "status": "Running",
        "created_at": "2026-01-01T00:00:00Z", "profiles": ["default"], "config": config,
        "devices": { "root": { "type": "disk", "path": "/", "pool": "pool1", "size": "60GiB" } },
    })
}

/// A fake serving `instances` in `default` and `bench`, the bridge `br1`, the default profile's `eth0` on it, and the lease
/// of `ADOPT_IP` for `ADOPT_MAC`.
pub fn adopt_fake(scratch: &Scratch, instances: Vec<Value>) -> FakeIncus {
    let state = FakeState {
        server: json!({ "api_version": "1.0" }),
        projects: vec![
            json!({ "name": "default", "description": "", "config": {} }),
            json!({ "name": "bench", "description": "", "config": {} }),
        ],
        instances,
        pools: vec![("pool1".to_string(), json!({}))],
        networks: vec![
            json!({ "name": "br1", "project": "default", "type": "bridge",
            "config": { "ipv4.address": "192.0.2.1/24" } }),
        ],
    };
    let fake = FakeIncus::start(&scratch.dir, state).expect("the fake serves");
    fake.add_profile(
        "default",
        json!({ "name": "default", "config": {},
            "devices": { "eth0": { "type": "nic", "name": "eth0", "network": "br1" } } }),
    );
    fake.set_lease("br1", ADOPT_MAC, ADOPT_IP);
    fake
}

/// An adoption config over `fake` that lists `svc1` and attaches `br1`, written into `scratch`, with its state directory made.
pub fn adopt_config(scratch: &Scratch, fake: &FakeIncus) -> std::path::PathBuf {
    let state = scratch.path("state");
    fs::create_dir_all(&state).expect("the state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("mode 0700");
    let config = json!({
        "stateDir": state.display().to_string(),
        "incus": { "socket": fake.socket().display().to_string() },
        "identityPhones": ["phone1"],
        "listen": { "unix": scratch.path("arca.sock").display().to_string() },
        "tokens": { "hashDir": scratch.path("tokens").display().to_string() },
        "kernel": {
            "projects": [ADOPT_TO], "droiddProjects": ["droid"],
            "adopt": [{ "instance": "svc1", "to": ADOPT_TO }], "attachNetworks": ["br1"],
        },
    });
    let path = scratch.path("adopt.json");
    fs::write(&path, config.to_string()).expect("the config");
    path
}

/// Runs `arca adopt <args> --config <config>` in this process.
pub fn adopt_run(config: &Path, args: &[&str]) -> Result<Option<Value>, arca::Failure> {
    let mut argv: Vec<String> = std::iter::once("adopt")
        .chain(args.iter().copied())
        .map(str::to_string)
        .collect();
    argv.push("--config".to_string());
    argv.push(config.display().to_string());
    arca::run(&argv)
}

/// The plan's digest, from `arca adopt plan`.
pub fn adopt_digest(config: &Path) -> String {
    let planned = adopt_run(
        config,
        &[
            "plan",
            "--instance",
            "svc1",
            "--expect-ip",
            ADOPT_IP,
            "--json",
        ],
    )
    .expect("the plan")
    .expect("a result");
    planned["digest"].as_str().expect("a digest").to_string()
}
