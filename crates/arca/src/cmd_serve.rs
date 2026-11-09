//! `serve --config <file>`: the Arca MCP server on the unix socket, and on the tailnet and proxy addresses when the config
//! names them.
//! The server runs until SIGTERM or SIGINT, then removes its own socket and exits 0.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use arca_alerts::Engine;
use arca_alerts::checks::CheckTargets;
use arca_alerts::rules::{self, Probes, RulesError};
use arca_api::verbs;
use arca_approval::{
    ApprovalRefusal, ApprovalVerifier, FileNonces, FileSignCounts, ForeignPasskeys, OwnerKeys,
    PASSKEY_COUNTS_FILE, PasskeyLine, PasskeyVerifier, STATE_PASSKEYS_FILE, SignerLine,
    SshSigVerifier, TEST_PASSKEYS_ENV, load_passkeys, load_signers, load_state_passkeys,
    passkeys_trust, signers_trust,
};
use arca_incus::{ProjectFamily, UnixIncus};
use arca_kernel::KernelError;
use arca_kernel::identity::NodeKey;
use arca_kernel::plugin::Provider;
use arca_mcp::{Server, TokenStore, TokenStoreError, serve_proxy, serve_tailnet, serve_unix};
use arca_metrics::ProbeTarget;
use arca_providers::ReadOnlyIncusProvider;
use arca_providers::incus::{Family, IncusProvider, RetainedSet};
use arca_reconcile::PluginSet;
use arca_store::Store;
use serde_json::Value;
use tokio::signal::unix::{SignalKind, signal};

use crate::adoptions::AdoptionRecord;
use crate::config::{self, NodeConfig};
use crate::handlers::{
    ActionExecutor, ActionSlot, AlertsExport, AlertsHandler, AlertsPublished, ApprovalStatus,
    Clock, JournalListHandler, MetricsExport, MetricsHandler, OntologyQuery, Published,
    SAMPLE_INTERVAL, StatusHandler, Ticker, TokenListHandler, WatchHandler, new_sampler, now_ms,
    plan_handlers_full, spawn_watcher,
};
use crate::kernel::Kernel;
use crate::{Failure, Flags};

/// The file that holds the node's ed25519 key, in the state directory.
const NODE_KEY_FILE: &str = "node.key";
/// The file a running server leaves its pid in, so `arca token revoke` can send it SIGHUP.
pub(crate) const SERVE_PID_FILE: &str = "serve.pid";
#[cfg(target_os = "linux")]
const PID_NOFOLLOW: i32 = 0o400000;
#[cfg(not(target_os = "linux"))]
const PID_NOFOLLOW: i32 = 0;
/// The file the approval nonces are spent in, in the state directory.
const NONCE_FILE: &str = "approval-nonces.jsonl";
/// The directory secret files are read from, in the state directory.
const SECRETS_DIR: &str = "secrets";
/// The scratch-only owner signers injection. The server reads its owner keys from this file only in a scratch kernel, and only in
/// a build that honours the injection.
const TEST_SIGNERS_ENV: &str = "ARCA_TEST_OWNER_SIGNERS";
/// The system's host name, which an approval statement names when the config names no approval host.
const SYSTEM_HOSTNAME: &str = "/proc/sys/kernel/hostname";

/// The `watch_report` handler of `cfg`. Each sweep reads the adoption record in the state directory again and counts its
/// adopted instances as live entries, so an adopted instance shows kernel-owned. A record that cannot be read counts none, and
/// says so on stderr: its adopted instances then show as drift, never as clean.
pub fn watch_handler(cfg: &NodeConfig) -> WatchHandler {
    let phones: BTreeSet<String> = cfg.identity_phones.iter().cloned().collect();
    let state_dir = cfg.state_dir.clone();
    WatchHandler::new(cfg.incus.socket.clone(), phones).with_adopted(move || {
        match AdoptionRecord::open(&state_dir) {
            Ok(record) => record.adopted(),
            Err(failure) => {
                eprintln!(
                    "arca: the adoption record cannot be read, so adopted instances show as drift: {}",
                    failure.message()
                );
                BTreeSet::new()
            }
        }
    })
}

/// The server watch also counts the shared kernel's service desired entries on every sweep.
pub fn watch_kernel_handler(cfg: &NodeConfig, kernel: Arc<Mutex<Kernel>>) -> WatchHandler {
    let base = watch_handler(cfg);
    let adopted = Arc::clone(&base.adopted);
    base.with_adopted(move || {
        let mut live = adopted();
        let kernel = kernel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        live.extend(
            kernel
                .entries()
                .iter()
                .filter(|entry| entry.plugin.0 == "arca.service")
                .map(|entry| entry.id.clone()),
        );
        live
    })
}

pub fn serve(flags: &Flags) -> Result<Option<Value>, Failure> {
    let cfg = config::load(Path::new(flags.required("config")?))?;
    if !cfg.kernel.adopt.is_empty() {
        // An adoption is never a desired-state entry, so no reconciler may run over a config that names one.
        return Err(Failure::Config(
            "serve refuses a config whose kernel.adopt is not empty: such a config is for arca adopt alone".to_string(),
        ));
    }
    let started = Instant::now();
    check_state_dir(&cfg.state_dir)?;

    // The alert rules are read before anything listens. A malformed alerts.json stops the start with exit 2.
    let alerts_file = rules::load(&cfg.state_dir).map_err(rules_failure)?;

    // The token store is read before anything listens. The server and the SIGHUP handler share it.
    let tokens = TokenStore::load(&cfg.tokens.hash_dir).map_err(token_failure)?;
    let key = NodeKey::load_or_create(&cfg.state_dir.join(NODE_KEY_FILE))
        .map_err(|e| Failure::Runtime(e.to_string()))?;
    let node = key.id().to_hex();

    // The kernel opens before anything listens, so a state directory that is held, or whose document is not this node's, stops
    // the start with nothing served.
    let host = approval_host(&cfg)?;
    // One value for the other kernels' passkeys files: the verifier reads them at each check, and arca_status reports that
    // read (its clone shares the last one).
    let foreign = foreign_passkeys(&cfg);
    let trusted = owner_keys(&cfg, foreign.clone())?;
    let (signing_keys, passkeys) = trusted.key_counts();
    eprintln!("arca: {signing_keys} owner signing key(s), {passkeys} passkey(s) kept");
    let nonces = FileNonces::open(&cfg.state_dir.join(NONCE_FILE)).map_err(approval_failure)?;
    let verifier = ApprovalVerifier::new(host.clone(), trusted, nonces);
    let mut plugins = PluginSet::new();
    plugins
        .add::<arca_netproject::NetProject>()
        .map_err(kernel_failure)?;
    plugins
        .add::<crate::service::Service>()
        .map_err(kernel_failure)?;
    plugins
        .add::<arca_postgres::Postgres>()
        .map_err(kernel_failure)?;
    plugins
        .add::<arca_postgres::consumer::Consumer>()
        .map_err(kernel_failure)?;
    let kernel_mode = !cfg.kernel.projects.is_empty();
    let kernel = Kernel::open(
        &cfg.state_dir,
        key,
        plugins,
        verifier,
        host,
        cfg.kernel.postgres_image.as_deref(),
        write_provider(&cfg, kernel_mode),
        now_ms(),
    )
    .map_err(kernel_failure)?;
    let applied_seq = kernel.applied_seq_reader();
    let kernel = Arc::new(Mutex::new(kernel));

    // The store opens before anything listens, so a store that refuses its pragmas stops the start with exit 1. The sampler
    // thread is its only writer; the handlers read it on their own read-only connection.
    let opened = Store::open(&cfg.state_dir, now_ms()).map_err(store_failure)?;
    let reader = Store::open_read_only(&cfg.state_dir).map_err(store_failure)?;
    let engine = Engine::start(
        opened.store,
        alerts_file.rules.clone(),
        opened.reset.is_some(),
        now_ms(),
    )
    .map_err(store_failure)?;

    // The sampler takes its first sample on its own thread, so the listeners start without waiting on Incus. The handlers read
    // the samples and the alerts it publishes and never take part in a read.
    // The watch sweep runs on its own thread, so a slow sweep never delays the tick. The tick reads its newest answer, and
    // `watch_report` answers from the same handler, so neither starts a sweep.
    let clock: Clock = Arc::new(now_ms);
    let watch = Arc::new(watch_kernel_handler(&cfg, Arc::clone(&kernel)));
    spawn_watcher(Arc::clone(&watch), Arc::clone(&clock))
        .map_err(|e| Failure::Runtime(e.to_string()))?;

    let published = Published::default();
    let alerts = AlertsPublished::default();
    let targets = CheckTargets {
        incus: cfg.incus.socket.clone(),
        arca: cfg.listen.unix.clone(),
        shim: alerts_file.shim_socket.clone(),
    };
    let (sampler, metrics_source) = new_sampler(&cfg);
    let sampler = sampler.with_probes(probe_targets(&alerts_file.probes));
    let ticker = Ticker::new(sampler, engine, published.clone(), alerts.clone())
        .with_checks(targets)
        .with_watch(Arc::clone(&watch));
    spawn_sampler(ticker).map_err(|e| Failure::Runtime(e.to_string()))?;

    let reader = Arc::new(Mutex::new(reader));
    let mode = if kernel_mode { "kernel" } else { "watch-only" };
    // action_approve is served only to a kernel set up to accept relayed approvals: an `approval` key that trusts another
    // kernel's passkeys file. Without one it is not in the served table, so tools/list never lists it and a call answers
    // not_found, exactly as before the verb existed.
    let relayed = accepts_relayed_approvals(&cfg);
    let mut server = Server::new(served_verbs(relayed), tokens.clone())
        .handle(
            "arca_status",
            Arc::new(StatusHandler {
                node: node.clone(),
                mode,
                applied_seq,
                started,
                socket: cfg.incus.socket.clone(),
                metrics_source: Arc::clone(&metrics_source),
                approval: cfg.approval.as_ref().map(|approval| ApprovalStatus {
                    linked_hosts: approval
                        .linked_hosts
                        .iter()
                        .map(|host| host.name.clone())
                        .collect(),
                    foreign: foreign.clone(),
                }),
            }),
        )
        .handle(
            "node_metrics",
            Arc::new(MetricsHandler {
                published: published.clone(),
            }),
        )
        .handle("watch_report", watch)
        .handle(
            "journal_list",
            Arc::new(JournalListHandler {
                state_dir: cfg.state_dir.clone(),
            }),
        )
        .handle(
            "token_list",
            Arc::new(TokenListHandler {
                tokens: tokens.clone(),
            }),
        )
        .handle("ontology_query", Arc::new(OntologyQuery))
        .handle(
            "alerts_list",
            Arc::new(AlertsHandler {
                alerts: alerts.clone(),
                published,
                store: Arc::clone(&reader),
                node: node.clone(),
            }),
        )
        .get_route(
            "/alerts",
            Arc::new(AlertsExport {
                node: node.clone(),
                store: reader,
            }),
        )
        .get_route("/metrics", Arc::new(MetricsExport { node, alerts }));
    // A watch-only node has no device writer. A kernel-mode node writes GPU devices only in its own project family, and
    // only reads local telemetry for the VM-in-use check when the metrics source is a settled local daemon.
    let gpu: Option<Arc<dyn crate::gpu::GpuBackend>> = if kernel_mode {
        let family = ProjectFamily::new(
            cfg.kernel.projects.iter().map(String::as_str),
            cfg.kernel.droidd_projects.iter().map(String::as_str),
        )
        .map_err(Failure::Config)?;
        let mode = Arc::clone(&metrics_source);
        let retained_kernel = Arc::clone(&kernel);
        let runtime = crate::gpu::GpuRuntime::new(
            move || {
                let choice = mode
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                choice.is_local() && !choice.provisional
            },
            move || {
                let kernel = retained_kernel
                    .lock()
                    .map_err(|_| "the kernel lock is poisoned".to_string())?;
                kernel.retained_entries().map_err(|error| error.to_string())
            },
        );
        Some(Arc::new(crate::gpu::IncusGpu::new(
            cfg.incus.socket.clone(),
            family,
            runtime,
        )))
    } else {
        None
    };
    // Snapshot storage reads the disks and pools on every node; only a kernel-mode node gets the snapshot writer, and it
    // writes only snapshot pools and the arcak-snapshots project.
    let storage = Arc::new(crate::storage::Storage::new(
        cfg.incus.socket.clone(),
        Arc::new(crate::storage::Lsblk),
        kernel_mode,
    ));
    server = server
        .handle(
            "storage_list",
            Arc::new(crate::storage::StorageListHandler {
                storage: Arc::clone(&storage),
            }),
        )
        .handle(
            "snapshot_list",
            Arc::new(crate::storage::SnapshotListHandler {
                storage: Arc::clone(&storage),
            }),
        );
    let action_slot = ActionSlot::default();
    let action_handlers: Vec<(&'static str, Arc<dyn arca_mcp::ToolHandler>)> = plan_handlers_full(
        Arc::clone(&kernel),
        clock.clone(),
        cfg.kernel.postgres_image.clone(),
        gpu,
        Some(storage),
        Arc::clone(&action_slot),
    )
    .into_iter()
    .chain(crate::handlers::postgres_handlers(
        kernel.clone(),
        clock.clone(),
        Arc::new(cfg.clone()),
    ))
    .chain(crate::handlers::service_handlers(
        kernel.clone(),
        clock.clone(),
        Arc::new(cfg.clone()),
    ))
    .collect();
    for (verb, handler) in &action_handlers {
        if *verb == "action_approve" && !relayed {
            continue;
        }
        server = server.handle(verb, Arc::clone(handler));
    }
    let actions = ActionExecutor::new(action_handlers);
    let _ = action_slot.set(actions.clone());
    let router = server.router();
    let page = cfg.approval_origin.clone().map(|origin| {
        crate::approve_page::Page::new(
            origin,
            Arc::clone(&kernel),
            clock.clone(),
            cfg.state_dir.clone(),
            Arc::new(crate::approve_page::DesktopDisplay),
        )
        .with_canvas_origin(&cfg.canvas_origin)
        .with_actions(actions)
        .with_linked_hosts(
            cfg.approval
                .as_ref()
                .map(|approval| approval.linked_hosts.clone())
                .unwrap_or_default(),
        )
    });
    let proxy_router = match &page {
        Some(page) => router.clone().merge(page.router(true)),
        None => router.clone(),
    };
    let router = match &page {
        Some(page) => router.merge(page.router(false)),
        None => router,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::Runtime(e.to_string()))?;
    let pid_path = cfg.state_dir.join(SERVE_PID_FILE);
    let unix_path = cfg.listen.unix.clone();
    let outcome: io::Result<()> = runtime.block_on(async {
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let reload_tokens = tokens.clone();
        let reload_dir = cfg.tokens.hash_dir.clone();
        tokio::spawn(async move {
            while hangup.recv().await.is_some() {
                match reload_tokens.reload(&reload_dir) {
                    Ok(count) => eprintln!("arca: {count} token(s) reloaded"),
                    Err(error) => eprintln!("arca: the token store was not reloaded: {error}"),
                }
            }
        });
        let pid_write_path = pid_path.clone();
        let pid_probe = unix_path.clone();
        tokio::spawn(async move {
            let until = Instant::now() + Duration::from_secs(10);
            while Instant::now() < until {
                if let Ok(stream) = tokio::net::UnixStream::connect(&pid_probe).await {
                    write_pid_for_peer(&stream, &pid_write_path);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let stop = async {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
        };
        // A listener the config does not name is a future that never ends, so the first listener to fail, or the first
        // signal, stops them all.
        let tailnet = async {
            match cfg.listen.tailnet {
                Some(addr) => serve_tailnet(router.clone(), addr).await,
                None => std::future::pending().await,
            }
        };
        let proxy = async {
            match cfg.listen.proxy {
                Some(proxy) => serve_proxy(proxy_router.clone(), proxy.addr, proxy.peer).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            result = serve_unix(router.clone(), &unix_path) => result,
            result = tailnet => result,
            result = proxy => result,
            () = stop => Ok(()),
        }
    });
    remove_own_pid(&pid_path);
    match outcome {
        Ok(()) => {
            remove_own_socket(&cfg.listen.unix);
            Ok(None)
        }
        Err(e) => Err(Failure::Runtime(format!("the server stopped: {e}"))),
    }
}

/// Publishes our pid only when the connected listener belongs to this process.
fn write_pid_for_peer(stream: &tokio::net::UnixStream, path: &Path) {
    if !stream.peer_cred().is_ok_and(|cred| {
        cred.pid()
            .is_some_and(|pid| pid as u32 == std::process::id())
    }) {
        return;
    }
    if let Ok(mut file) = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(PID_NOFOLLOW)
        .open(path)
        && file.metadata().is_ok_and(|meta| meta.is_file())
    {
        let _ = file.write_all(format!("{}\n", std::process::id()).as_bytes());
        let _ = file.sync_all();
    }
}

/// Removes `path` only when it still holds this process's pid, so a second server cannot delete the first one's file.
fn remove_own_pid(path: &Path) {
    let own = std::process::id().to_string();
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_file() {
        return;
    }
    if fs::read_to_string(path)
        .ok()
        .is_some_and(|text| text.trim() == own)
    {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod pid_tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arca-pid-{}-{}-{tag}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::create_dir_all(&dir).expect("scratch");
        fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("0700");
        dir
    }

    #[tokio::test]
    async fn our_listener_peer_writes_our_pid_with_private_permissions() {
        let dir = scratch("own-peer");
        let socket = dir.join("serve.sock");
        let _listener = tokio::net::UnixListener::bind(&socket).expect("own listener");
        let stream = tokio::net::UnixStream::connect(&socket)
            .await
            .expect("connect");
        let path = dir.join(SERVE_PID_FILE);
        write_pid_for_peer(&stream, &path);
        assert_eq!(
            fs::read_to_string(&path).expect("pid"),
            format!("{}\n", std::process::id())
        );
        assert_eq!(fs::metadata(&path).expect("metadata").mode() & 0o777, 0o600);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_second_server_pid_file_is_not_removed_by_this_process() {
        let dir = scratch("remove");
        let path = dir.join(SERVE_PID_FILE);
        fs::write(&path, "999999\n").expect("other pid");
        remove_own_pid(&path);
        assert!(path.exists(), "the other server's pid file stays");
        fs::write(&path, format!("{}\n", std::process::id())).expect("own pid");
        remove_own_pid(&path);
        assert!(!path.exists(), "this process removes its own pid file");
        let _ = fs::remove_dir_all(dir);
    }
}

/// The state directory must be a real directory, not a link, owned by this user, and private to its owner (mode 0700). It is
/// checked before the node key is created in it, so a key is never written into a directory another user can read.
fn check_state_dir(dir: &Path) -> Result<(), Failure> {
    let uid = arca_approval::effective_uid().map_err(approval_failure)?;
    check_state_dir_owned_by(dir, uid)
}

/// [`check_state_dir`] for a directory that must be owned by `uid`.
fn check_state_dir_owned_by(dir: &Path, uid: u32) -> Result<(), Failure> {
    let meta = fs::symlink_metadata(dir).map_err(|_| {
        Failure::Config(format!("the state directory {} is missing", dir.display()))
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(Failure::Config(format!(
            "the state directory {} is not a directory",
            dir.display()
        )));
    }
    if meta.uid() != uid {
        return Err(Failure::Config(format!(
            "the state directory {} is not owned by this user",
            dir.display()
        )));
    }
    if meta.permissions().mode() & 0o777 != 0o700 {
        return Err(Failure::Config(format!(
            "the state directory {} must be mode 0700",
            dir.display()
        )));
    }
    Ok(())
}

/// The owner keys this kernel trusts for the approval namespace: the SSH hardware keys of the signers file and the passkeys
/// of the passkeys file, with the passkeys' sign counts in the state directory. A signers file that cannot be trusted stops
/// the start. A passkeys file that cannot be trusted or read refuses every passkey approval, and the SSH keys still verify.
/// Its refusal names the file, the rule and at most a line number, never a key. A test passkeys file named outside a scratch
/// kernel stops the start.
fn owner_keys(cfg: &NodeConfig, foreign: ForeignPasskeys) -> Result<OwnerKeys, Failure> {
    let ssh = SshSigVerifier::new(owner_signers(cfg)?);
    let test: Option<PathBuf> = std::env::var_os(TEST_PASSKEYS_ENV).map(PathBuf::from);
    let (path, trust) =
        passkeys_trust(cfg.kernel.scratch, &cfg.kernel.projects, test).map_err(approval_failure)?;
    let counts =
        FileSignCounts::open(&cfg.state_dir.join(PASSKEY_COUNTS_FILE)).map_err(approval_failure)?;
    let loaded = (|| -> Result<Vec<PasskeyLine>, ApprovalRefusal> {
        let mut lines = load_passkeys(&path, &trust)?;
        let state_path = cfg.state_dir.join(STATE_PASSKEYS_FILE);
        let state_lines = load_state_passkeys(&state_path)?;
        for line in state_lines {
            if lines.iter().any(|seen| {
                seen.credential_id == line.credential_id || seen.fingerprint == line.fingerprint
            }) {
                return Err(ApprovalRefusal::Untrusted(
                    "a credential id or public key is in both passkeys files".to_string(),
                ));
            }
            lines.push(line);
        }
        Ok(lines)
    })();
    let passkeys = match loaded {
        Ok(lines) => PasskeyVerifier::new(lines, counts).with_foreign(foreign),
        Err(refusal) => {
            eprintln!("arca: every passkey approval is refused: {refusal}");
            PasskeyVerifier::refusing(refusal)
        }
    };
    Ok(OwnerKeys::new(ssh, passkeys))
}

/// The other kernels' passkeys files this kernel also trusts for approvals, from `approval.trustOwnerKeysFrom`, for the
/// origins `approval.origins` lists. No `approval` key trusts none. Each file's state at start is logged by path and count
/// only; `arca_status` reports the latest read by index and file name.
fn foreign_passkeys(cfg: &NodeConfig) -> ForeignPasskeys {
    let Some(approval) = &cfg.approval else {
        return ForeignPasskeys::default();
    };
    let foreign = ForeignPasskeys::new(
        approval.trust_owner_keys_from.clone(),
        approval.origins.clone(),
    );
    for state in foreign.load().1 {
        match &state.reason {
            None => eprintln!(
                "arca: {} owner passkey(s) trusted from {}",
                state.keys,
                state.path.display()
            ),
            Some(reason) => eprintln!(
                "arca: no owner passkey is trusted from {}: {reason}",
                state.path.display()
            ),
        }
    }
    foreign
}

/// Whether this kernel accepts approvals relayed from another kernel's page: its config has an `approval` key that names at
/// least one passkeys file to trust. Only then is `action_approve` served.
fn accepts_relayed_approvals(cfg: &NodeConfig) -> bool {
    cfg.approval
        .as_ref()
        .is_some_and(|approval| !approval.trust_owner_keys_from.is_empty())
}

/// The verbs this kernel serves: the whole table when it accepts relayed approvals, else the table without
/// `action_approve`. The smaller table is built once at start and lives as long as the process.
fn served_verbs(relayed: bool) -> &'static [arca_api::Verb] {
    if relayed {
        return verbs();
    }
    let table: Vec<arca_api::Verb> = verbs()
        .iter()
        .filter(|verb| verb.name != "action_approve")
        .copied()
        .collect();
    Box::leak(table.into_boxed_slice())
}

/// The owner signing keys this kernel trusts for the approval namespace. A signers file that is not there is no key at all, so
/// every approval is refused. Only the count of kept keys is ever logged, by the caller, never the keys.
fn owner_signers(cfg: &NodeConfig) -> Result<Vec<SignerLine>, Failure> {
    let test: Option<PathBuf> = std::env::var_os(TEST_SIGNERS_ENV).map(PathBuf::from);
    let (path, trust) =
        signers_trust(cfg.kernel.scratch, &cfg.kernel.projects, test).map_err(approval_failure)?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        _ => load_signers(&path, &trust).map_err(approval_failure),
    }
}

/// The host name the approval statements name: the config's approval host when one is set, else the system's host name.
fn approval_host(cfg: &NodeConfig) -> Result<String, Failure> {
    if let Some(host) = &cfg.approval_host {
        return Ok(host.clone());
    }
    let text = fs::read_to_string(SYSTEM_HOSTNAME)
        .map_err(|_| Failure::Runtime("the system host name cannot be read".to_string()))?;
    let host = text.trim().to_string();
    if !config::host_name_ok(&host) {
        return Err(Failure::Config(
            "the system host name is not a host name; set approvalHost in the config".to_string(),
        ));
    }
    Ok(host)
}

/// The provider the reconciler writes through. A node with kernel projects writes through the Incus writer, which serves only
/// those projects. A node with none reads only, so no removal can reach Incus.
fn write_provider(
    cfg: &NodeConfig,
    kernel_mode: bool,
) -> impl FnOnce(RetainedSet) -> Result<Box<dyn Provider + Send>, KernelError> {
    let socket = cfg.incus.socket.clone();
    let secrets = cfg.state_dir.join(SECRETS_DIR);
    let family = Family {
        projects: cfg.kernel.projects.clone(),
        scratch: cfg.kernel.scratch,
        droidd_projects: cfg.kernel.droidd_projects.clone(),
        reserved_cidrs: cfg.kernel.reserved_cidrs.clone(),
    };
    move |retained| {
        if kernel_mode {
            let provider = IncusProvider::new(socket, family, secrets, retained)
                .map_err(|refusal| KernelError::Plugin(refusal.to_string()))?;
            return Ok(Box::new(provider));
        }
        Ok(Box::new(ReadOnlyIncusProvider::new(UnixIncus::new(socket))))
    }
}

/// A kernel that will not open, as a start's failure. A provider the family refuses is a config failure; a held state directory
/// or a state that does not verify is a runtime failure.
fn kernel_failure(error: KernelError) -> Failure {
    match error {
        KernelError::Plugin(text) => Failure::Config(text),
        KernelError::Locked { .. } => {
            Failure::Runtime("the state directory is held by another arca process".to_string())
        }
        other => Failure::Runtime(other.to_string()),
    }
}

/// An approval refusal at start, as a failure of start. A file that cannot be trusted is a config failure.
fn approval_failure(error: ApprovalRefusal) -> Failure {
    match error {
        ApprovalRefusal::Untrusted(_) => Failure::Config(error.to_string()),
        other => Failure::Runtime(other.to_string()),
    }
}

/// The token store's refusals as exits. A directory that cannot be read is an I/O failure (exit 1). A directory or file that
/// is unsafe, missing or malformed is a config failure (exit 2), and the message says what to change.
fn token_failure(error: TokenStoreError) -> Failure {
    match &error {
        TokenStoreError::Io { .. } => Failure::Runtime(error.to_string()),
        TokenStoreError::UnsafeDir { reason } => Failure::Config(format!(
            "the token directory {reason}; use mode 0700 or 0500, owned by this user"
        )),
        _ => Failure::Config(error.to_string()),
    }
}

/// A store that refuses to open or start, as a failure of start (exit 1).
fn store_failure(error: arca_store::StoreError) -> Failure {
    Failure::Runtime(error.to_string())
}

/// An `alerts.json` that cannot be read or is refused, as a config failure (exit 2). The message names a key, never a value.
fn rules_failure(error: RulesError) -> Failure {
    Failure::Config(error.to_string())
}

/// The latency targets `alerts.json` names, in the order the gauge writes them. A target that is not named is not probed.
fn probe_targets(probes: &Probes) -> Vec<(ProbeTarget, SocketAddr)> {
    [
        (ProbeTarget::Gateway, probes.gateway),
        (ProbeTarget::Internet, probes.internet),
        (ProbeTarget::Relay, probes.relay),
    ]
    .into_iter()
    .filter_map(|(target, addr)| addr.map(|addr| (target, addr)))
    .collect()
}

/// Ticks on a background thread, first at once and then every [`SAMPLE_INTERVAL`], for the life of the process. Each tick's
/// sample and alerts are published as soon as they are taken.
fn spawn_sampler(mut ticker: Ticker<Store>) -> io::Result<()> {
    thread::Builder::new()
        .name("arca-sampler".to_string())
        .spawn(move || {
            loop {
                ticker.tick(now_ms());
                thread::sleep(SAMPLE_INTERVAL);
            }
        })?;
    Ok(())
}

/// Removes the server's socket after it stops, but only when nothing answers on it. A socket another server listens on is
/// left alone.
fn remove_own_socket(path: &Path) {
    let is_socket = fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_socket());
    let refused = matches!(
        UnixStream::connect(path),
        Err(ref e) if e.kind() == io::ErrorKind::ConnectionRefused
    );
    if is_socket && refused {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A directory under the default temporary directory, mode 0700, removed on drop.
    struct StateDir(PathBuf);

    impl StateDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("arca-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir(&dir).expect("the directory is created");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
            Self(dir)
        }
    }

    impl Drop for StateDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn only_the_named_probe_targets_are_probed_and_in_the_gauge_order() {
        let gateway: SocketAddr = "127.0.0.1:9".parse().expect("a socket address");
        let relay: SocketAddr = "127.0.0.2:9".parse().expect("a socket address");
        let probes = Probes {
            gateway: Some(gateway),
            internet: None,
            relay: Some(relay),
        };
        assert_eq!(
            probe_targets(&probes),
            vec![(ProbeTarget::Gateway, gateway), (ProbeTarget::Relay, relay)]
        );
        assert!(probe_targets(&Probes::default()).is_empty());
    }

    #[test]
    fn a_state_dir_owned_by_another_uid_is_refused_and_one_this_user_owns_passes() {
        // Only root can give a directory to another uid, so the check is run against a uid the directory does not carry.
        let dir = StateDir::new("state-owner");
        let me = arca_approval::effective_uid().expect("the effective uid is read");
        let refused = check_state_dir_owned_by(&dir.0, me.wrapping_add(1));
        assert!(
            matches!(refused, Err(Failure::Config(_))),
            "a directory another uid owns is refused: {refused:?}"
        );
        check_state_dir_owned_by(&dir.0, me).expect("a directory this user owns passes");
    }
}
