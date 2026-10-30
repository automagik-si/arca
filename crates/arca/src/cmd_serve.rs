//! `serve --config <file>`: the Arca MCP server on the unix socket, and on the tailnet address when the config names one.
//! The server runs until SIGTERM or SIGINT, then removes its own socket and exits 0.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use arca_api::verbs;
use arca_incus::UnixIncus;
use arca_kernel::identity::NodeKey;
use arca_mcp::{Server, TokenStore, TokenStoreError, serve_tailnet, serve_unix};
use arca_metrics::Sampler;
use serde_json::Value;
use tokio::signal::unix::{SignalKind, signal};

use crate::config;
use crate::handlers::{
    MetricsHandler, Published, SAMPLE_INTERVAL, StatusHandler, TokenListHandler, WatchHandler,
    new_sampler, sample_and_publish,
};
use crate::{Failure, Flags};

/// The file that holds the node's ed25519 key, in the state directory.
const NODE_KEY_FILE: &str = "node.key";

pub fn serve(flags: &Flags) -> Result<Option<Value>, Failure> {
    let cfg = config::load(Path::new(flags.required("config")?))?;
    let started = Instant::now();
    if !cfg.state_dir.is_dir() {
        return Err(Failure::Config(format!(
            "the state directory {} is missing",
            cfg.state_dir.display()
        )));
    }

    // The token store is read before anything listens. Its ids are copied out first, because the server takes the store.
    let tokens = TokenStore::load(&cfg.tokens.hash_dir).map_err(token_failure)?;
    let ids = tokens.ids();
    let key = NodeKey::load_or_create(&cfg.state_dir.join(NODE_KEY_FILE))
        .map_err(|e| Failure::Runtime(e.to_string()))?;

    // The sampler takes its first sample on its own thread, so the listeners start without waiting on Incus. The handlers read
    // the samples it publishes and never take part in a read.
    let published = Published::default();
    spawn_sampler(new_sampler(&cfg), published.clone())
        .map_err(|e| Failure::Runtime(e.to_string()))?;

    let phones: BTreeSet<String> = cfg.identity_phones.iter().cloned().collect();
    let server = Server::new(verbs(), tokens)
        .handle(
            "arca_status",
            Arc::new(StatusHandler {
                node: key.id().to_hex(),
                started,
                socket: cfg.incus.socket.clone(),
            }),
        )
        .handle("node_metrics", Arc::new(MetricsHandler { published }))
        .handle(
            "watch_report",
            Arc::new(WatchHandler {
                socket: cfg.incus.socket.clone(),
                phones,
            }),
        )
        .handle("token_list", Arc::new(TokenListHandler { ids }));
    let router = server.router();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::Runtime(e.to_string()))?;
    let unix_path = cfg.listen.unix.clone();
    let outcome: io::Result<()> = runtime.block_on(async {
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let stop = async {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
        };
        match cfg.listen.tailnet {
            Some(addr) => {
                tokio::select! {
                    result = serve_unix(router.clone(), &unix_path) => result,
                    result = serve_tailnet(router, addr) => result,
                    () = stop => Ok(()),
                }
            }
            None => {
                tokio::select! {
                    result = serve_unix(router, &unix_path) => result,
                    () = stop => Ok(()),
                }
            }
        }
    });
    match outcome {
        Ok(()) => {
            remove_own_socket(&cfg.listen.unix);
            Ok(None)
        }
        Err(e) => Err(Failure::Runtime(format!("the server stopped: {e}"))),
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

/// Samples the host on a background thread, first at once and then every [`SAMPLE_INTERVAL`], for the life of the process. Each
/// sample is published as soon as it is taken.
fn spawn_sampler(mut sampler: Sampler<UnixIncus>, published: Published) -> io::Result<()> {
    thread::Builder::new()
        .name("arca-sampler".to_string())
        .spawn(move || {
            loop {
                sample_and_publish(&mut sampler, &published);
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
