use crate::{
    Config, Error, FORWARDED_STREAM_LIMIT, PAIR_ALPN, PEERS_ALPN, PairLink, Peer, Result, State,
    Status,
    guard::{Guard, Slot},
    mesh,
    pairing::{self, Request, Response, read_line, write_line},
    runtime,
};
use iroh::{
    Endpoint, EndpointAddr, RelayMode,
    endpoint::{Connection, Incoming, presets},
};
use iroh_tickets::endpoint::EndpointTicket;
use std::{
    collections::BTreeMap,
    fs::File,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::{Notify, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout, timeout_at},
};

/// Production uses N0 discovery and relays; tests can bind only a local direct socket.
#[derive(Clone, Copy, Default)]
pub enum Network {
    #[default]
    N0,
    Direct(SocketAddr),
}
impl Network {
    pub fn local() -> Self {
        Self::Direct((Ipv4Addr::LOCALHOST, 0).into())
    }
}

type Connections = Arc<Mutex<BTreeMap<usize, Connection>>>;

pub struct Link {
    endpoint: Endpoint,
    state: State,
    name: String,
    stop: watch::Sender<bool>,
    refresh: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl Link {
    pub async fn start(state: State, config: Config, network: Network) -> Result<Self> {
        config.check()?;
        let lock = state.open_lock(".link-serve.lock")?;
        lock.try_lock()
            .map_err(|_| Error::Runtime("link serve is already running"))?;
        let cleanup = Cleanup {
            state: state.clone(),
            _lock: lock,
        };
        state.delete("status.json")?;
        let key = state.secret()?;
        let mut alpns = vec![PAIR_ALPN.to_vec(), PEERS_ALPN.to_vec()];
        alpns.extend(config.forward.keys().map(|a| a.as_bytes().to_vec()));
        let builder = match network {
            Network::N0 => Endpoint::builder(presets::N0),
            Network::Direct(addr) => runtime(
                Endpoint::builder(presets::Minimal)
                    .relay_mode(RelayMode::Disabled)
                    .clear_ip_transports()
                    .bind_addr(addr),
            )?,
        };
        let endpoint = runtime(builder.secret_key(key).alpns(alpns).bind().await)?;
        let name = config
            .name
            .clone()
            .unwrap_or_else(|| endpoint.id().to_string()[..10].to_string());
        let socket_path = state.dir.join(".link-control.sock");
        state.delete(".link-control.sock")?;
        let listener = runtime(UnixListener::bind(&socket_path))?;
        use std::os::unix::fs::PermissionsExt;
        runtime(std::fs::set_permissions(
            &socket_path,
            std::fs::Permissions::from_mode(0o600),
        ))?;
        publish(&state, &endpoint, &name)?;
        let (stop, receiver) = watch::channel(false);
        let refresh = Arc::new(Notify::new());
        let task = tokio::spawn(run(
            endpoint.clone(),
            state.clone(),
            name.clone(),
            config,
            listener,
            receiver,
            refresh.clone(),
            cleanup,
        ));
        Ok(Self {
            endpoint,
            state,
            name,
            stop,
            refresh,
            task: Some(task),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }
    pub async fn join(&self, link: &PairLink) -> Result<Peer> {
        let peer = pairing::join(&self.endpoint, &self.state, &self.name, link).await?;
        self.refresh.notify_one();
        Ok(peer)
    }
    /// Exchange full membership snapshots; unreachable members never fail a sync.
    pub async fn sync_peers(&self) -> Result<()> {
        mesh::sync(&self.endpoint, &self.state).await?;
        self.refresh.notify_one();
        Ok(())
    }
    /// Wake the removal sweep immediately, including in-process removals.
    pub fn refresh_peers(&self) {
        self.refresh.notify_one();
    }
    pub async fn stopped(&mut self) {
        if let Some(task) = self.task.as_mut() {
            let _ = task.await;
        }
        self.task = None;
    }
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

struct Cleanup {
    state: State,
    _lock: File,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.state.delete("status.json");
        let _ = self.state.delete(".link-control.sock");
    }
}
fn publish(state: &State, endpoint: &Endpoint, name: &str) -> Result<()> {
    state.write(
        "status.json",
        &Status {
            pid: std::process::id(),
            id: endpoint.id().to_string(),
            ticket: EndpointTicket::new(endpoint.addr()).to_string(),
            name: name.to_string(),
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn run(
    endpoint: Endpoint,
    state: State,
    name: String,
    config: Config,
    listener: UnixListener,
    mut stop: watch::Receiver<bool>,
    refresh: Arc<Notify>,
    _cleanup: Cleanup,
) {
    let guard = Guard::default();
    let connections: Connections = Arc::default();
    let mut tasks = JoinSet::new();
    let mut poll = tokio::time::interval(Duration::from_secs(2));
    let mut periodic = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(30),
        Duration::from_secs(30),
    );
    let mut syncing = JoinSet::new();
    let mut needs_sync = true;
    let mut last_removed = state.snapshot().ok().map(|s| s.removed);
    loop {
        if needs_sync && syncing.is_empty() {
            needs_sync = false;
            let (endpoint, state, refresh) = (endpoint.clone(), state.clone(), refresh.clone());
            let connections = connections.clone();
            syncing.spawn(async move {
                if matches!(mesh::sync(&endpoint, &state).await, Ok(true)) {
                    refresh.notify_one();
                }
                sweep(&state, &connections);
            });
        }
        tokio::select! {
            _ = stop.changed() => break,
            _ = refresh.notified() => { sweep(&state, &connections); needs_sync = true; },
            _ = periodic.tick() => needs_sync = true,
            _ = syncing.join_next(), if !syncing.is_empty() => {},
            _ = poll.tick() => {
                sweep(&state, &connections);
                let removed = state.snapshot().ok().map(|s| s.removed);
                if removed != last_removed { last_removed = removed; needs_sync = true; }
                if publish(&state, &endpoint, &name).is_err() { break; }
            },
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                // No slot is consumed until QUIC has validated the source address.
                if !incoming.remote_addr_validated() { let _ = incoming.retry(); continue; }
                let Some(slot) = guard.take(incoming.remote_addr()) else { incoming.refuse(); continue; };
                let deadline = Instant::now() + Duration::from_secs(10);
                tasks.spawn(handle(incoming, slot, state.clone(), endpoint.id().to_string(), config.clone(), connections.clone(), refresh.clone(), deadline));
            },
            client = listener.accept() => {
                if let Ok((client, _)) = client {
                    let (endpoint, state, name) = (endpoint.clone(), state.clone(), name.clone());
                    let (connections, refresh) = (connections.clone(), refresh.clone());
                    tasks.spawn(async move { let _ = control(client, endpoint, state, name, connections, refresh).await; });
                }
            },
            _ = tasks.join_next(), if !tasks.is_empty() => {},
        }
    }
    syncing.abort_all();
    while syncing.join_next().await.is_some() {}
    endpoint.close().await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

fn sweep(state: &State, connections: &Connections) {
    let Ok(peers) = state.peers() else {
        return;
    };
    for connection in connections.lock().expect("connections lock").values() {
        if !peers
            .iter()
            .any(|p| p.id == connection.remote_id().to_string())
        {
            connection.close(1u32.into(), b"not paired");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle(
    incoming: Incoming,
    mut slot: Slot,
    state: State,
    own_id: String,
    config: Config,
    connections: Connections,
    refresh: Arc<Notify>,
    deadline: Instant,
) {
    let remote_addr = incoming.remote_addr();
    let Ok(accepting) = incoming.accept() else {
        return;
    };
    // Retain a close handle during TLS, but never read application data before authentication.
    let early = accepting.into_0rtt();
    let mut registered = None;
    let prepare = async {
        let connection = runtime(early.handshake_completed().await)?;
        let is_pair = connection.alpn() == PAIR_ALPN;
        if !is_pair {
            if !state.admit(&connection.remote_id().to_string())? {
                connection.close(1u32.into(), b"not paired");
                return Err(Error::Runtime("not paired"));
            }
            let id = connection.stable_id();
            connections
                .lock()
                .expect("connections lock")
                .insert(id, connection.clone());
            registered = Some(id);
        }
        let (mut send, mut recv) = runtime(connection.accept_bi().await)?;
        if !is_pair {
            slot.trust();
        }
        if is_pair {
            let line = read_line(&mut recv).await?;
            let response = match serde_json::from_slice::<Request>(&line) {
                Ok(request) => state.redeem(
                    connection.remote_id(),
                    request,
                    Some(
                        EndpointTicket::new(EndpointAddr::from_parts(
                            connection.remote_id(),
                            [remote_addr.into()],
                        ))
                        .to_string(),
                    ),
                )?,
                Err(_) => Response::refused("invalid request"),
            };
            if response.ok {
                slot.trust();
                let id = connection.stable_id();
                connections
                    .lock()
                    .expect("connections lock")
                    .insert(id, connection.clone());
                registered = Some(id);
            }
            write_line(&mut send, &response).await?;
            runtime(send.finish())?;
            if response.ok {
                refresh.notify_one();
            }
            Ok((connection, None))
        } else if connection.alpn() == PEERS_ALPN {
            if mesh::receive(&state, &own_id, &mut send, &mut recv).await? {
                sweep(&state, &connections);
                refresh.notify_one();
            }
            Ok((connection, None))
        } else {
            let alpn = std::str::from_utf8(connection.alpn())
                .map_err(|_| Error::Runtime("invalid ALPN"))?;
            let path = config
                .forward
                .get(alpn)
                .cloned()
                .ok_or(Error::Runtime("unknown ALPN"))?;
            Ok((connection, Some((path, send, recv))))
        }
    };
    match timeout_at(deadline, prepare).await {
        Err(_) => early.close(1u32.into(), b"too slow"),
        Ok(Err(_)) => early.close(1u32.into(), b"link refused"),
        Ok(Ok((connection, None))) => {
            // Keep the reply alive until acknowledged or the requester closes.
            let _ = timeout(Duration::from_secs(10), connection.closed()).await;
            connection.close(0u32.into(), b"done");
        }
        Ok(Ok((connection, Some((path, send, recv))))) => {
            let mut streams = JoinSet::new();
            streams.spawn(forward(path.clone(), send, recv));
            loop {
                tokio::select! {
                    _ = connection.closed() => break,
                    stream = connection.accept_bi(), if streams.len() < FORWARDED_STREAM_LIMIT => match stream {
                        Ok((send, recv)) => { streams.spawn(forward(path.clone(), send, recv)); },
                        Err(_) => break,
                    },
                    _ = streams.join_next(), if !streams.is_empty() => {},
                }
            }
            streams.abort_all();
            while streams.join_next().await.is_some() {}
        }
    }
    if let Some(id) = registered {
        connections.lock().expect("connections lock").remove(&id);
    }
}

async fn forward(
    path: std::path::PathBuf,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
) {
    let Ok(mut socket) = UnixStream::connect(path).await else {
        return;
    };
    let (mut read, mut write) = socket.split();
    let upstream = async {
        let result = tokio::io::copy(&mut recv, &mut write).await;
        let _ = write.shutdown().await;
        result
    };
    let downstream = async {
        let result = tokio::io::copy(&mut read, &mut send).await;
        let _ = send.finish();
        result
    };
    let _ = tokio::try_join!(upstream, downstream);
}

// A same-user local request lets the CLI reuse the running endpoint and its key.
async fn control(
    mut socket: UnixStream,
    endpoint: Endpoint,
    state: State,
    name: String,
    connections: Connections,
    refresh: Arc<Notify>,
) -> Result<()> {
    let operation = async {
        let text = read_line(&mut socket).await?;
        let remove = serde_json::from_slice::<RemoveRequest>(&text).ok();
        let reply = if let Some(request) = remove {
            match state.remove(&request.remove) {
                Ok(count) => {
                    sweep(&state, &connections);
                    refresh.notify_one();
                    serde_json::json!({"removed": count})
                }
                Err(_) => serde_json::json!({"error": "removal failed"}),
            }
        } else {
            let result = match std::str::from_utf8(&text)
                .ok()
                .and_then(|s| PairLink::parse(s).ok())
            {
                Some(link) => pairing::join(&endpoint, &state, &name, &link).await,
                None => Err(Error::InvalidLink),
            };
            result
                .map(|p| {
                    refresh.notify_one();
                    serde_json::json!({"id": p.id, "name": p.name})
                })
                .unwrap_or_else(|_| serde_json::json!({"error": "pairing failed"}))
        };
        write_line(&mut socket, &reply).await
    };
    runtime(timeout(Duration::from_secs(12), operation).await)?
}

/// Join through the existing node endpoint when `serve` holds the node lock.
pub async fn join_running(state: &State, text: &str) -> Result<Peer> {
    PairLink::parse(text)?;
    state.status()?;
    let operation = async {
        let mut socket = runtime(UnixStream::connect(state.dir.join(".link-control.sock")).await)?;
        runtime(socket.write_all(text.as_bytes()).await)?;
        runtime(socket.write_all(b"\n").await)?;
        let value: serde_json::Value =
            runtime(serde_json::from_slice(&read_line(&mut socket).await?))?;
        let (Some(id), Some(name)) = (value["id"].as_str(), value["name"].as_str()) else {
            return Err(Error::Runtime("pairing failed"));
        };
        state
            .peers()?
            .into_iter()
            .find(|p| p.id == id && p.name == name)
            .ok_or(Error::Runtime("paired peer is absent"))
    };
    runtime(timeout(Duration::from_secs(15), operation).await)?
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct RemoveRequest {
    remove: String,
}

/// Remove through the running server so admission and existing streams change immediately.
pub async fn remove_running(state: &State, name: &str) -> Result<usize> {
    state.status()?;
    let operation = async {
        let mut socket = runtime(UnixStream::connect(state.dir.join(".link-control.sock")).await)?;
        write_line(
            &mut socket,
            &RemoveRequest {
                remove: name.to_string(),
            },
        )
        .await?;
        let value: serde_json::Value =
            runtime(serde_json::from_slice(&read_line(&mut socket).await?))?;
        value["removed"]
            .as_u64()
            .map(|n| n as usize)
            .ok_or(Error::Runtime("removal failed"))
    };
    runtime(timeout(Duration::from_secs(15), operation).await)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sweep_skips_read_errors_and_revokes_after_a_successful_read() {
        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let dir = std::env::temp_dir().join(format!("arca-sweep-{}", client.id()));
        let state = State::new(&dir).unwrap();
        let link = Link::start(state.clone(), Config::default(), Network::local())
            .await
            .unwrap();
        let connection = client
            .connect(
                EndpointAddr::new(link.endpoint().id())
                    .with_ip_addr(link.endpoint().bound_sockets()[0]),
                PAIR_ALPN,
            )
            .await
            .unwrap();
        let connections: Connections = Arc::default();
        connections
            .lock()
            .unwrap()
            .insert(connection.stable_id(), connection.clone());
        std::fs::write(dir.join("peers.json"), b"{").unwrap();
        sweep(&state, &connections);
        assert!(
            connection.close_reason().is_none(),
            "a state read error must preserve connections"
        );
        state.write("peers.json", &Vec::<Peer>::new()).unwrap();
        sweep(&state, &connections);
        assert!(
            connection.close_reason().is_some(),
            "a readable empty peer list must revoke connections"
        );
        link.shutdown().await;
        client.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}
