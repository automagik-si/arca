//! A fake Incus for tests. It holds the objects of a [`FakeState`] in memory and serves them on a unix socket. It records every
//! request it receives.
//!
//! GET routes: `/1.0`, `/1.0/projects?recursion=1`, `/1.0/instances?all-projects=true&recursion=1`,
//! `/1.0/instances?all-projects=true&recursion=2`, `/1.0/storage-pools?recursion=1`, `/1.0/storage-pools/<pool>/resources`,
//! `/1.0/networks?recursion=1` and `/1.0/networks/<name>/state` (the host's interfaces, set by
//! [`FakeIncus::set_host_interfaces`]), every object path, `/1.0/operations?all-projects=true&recursion=1`, and
//! `/1.0/operations/<id>` with its `/wait`. Any other GET is 404.
//!
//! The `recursion=2` instance listing is the `recursion=1` rows, each with a `state` object. A running instance's `state` holds
//! `cpu.usage` (nanoseconds) and `memory.usage` (bytes), set by [`FakeIncus::set_instance_usage`] and zero until a test sets them.
//! A stopped instance's `state` is `null`. [`FakeIncus::set_last_used`] sets an instance's `last_used_at`, as a start does.
//!
//! Operations: the listing groups every operation the fake holds by its status in lower case, as Incus does. A held operation
//! ([`FakeIncus::hold_operation`], [`FakeIncus::hold_create`]) stays running (status code 103) and names its object in
//! `resources`, rendered as Incus renders it: the object's path, with `?project=<p>` unless the project is `default`. It ends
//! only when the test releases it ([`FakeIncus::release_operation`]) or fails it ([`FakeIncus::fail_operation`]). A held
//! operation is of class `task` unless the test gives it the class and the description Incus gives an operation of another type
//! ([`FakeIncus::hold_operation_as`]). A wait on a running operation is held until the operation ends or its `timeout` passes,
//! but never longer than a quarter second, so a test is not slowed; a caller loops on a wait anyway, since Incus answers a
//! timed-out wait with the operation still running. [`FakeIncus::answer_wait_ended`] makes a wait disagree with the listing.
//!
//! A network, a forward and an ACL are served from the default project unless their project enables `features.networks`, as Incus
//! 7.3 serves them: a create sent to such a project lands in the default project, and a read sent to it is answered from there.
//!
//! Images live in an image store: a project's own when its config sets `features.images` to `"true"`, and the default project's
//! otherwise. `/1.0/images?project=<p>&recursion=1` lists the store the project reads, and `/1.0/images/<fingerprint>` is read
//! and deleted there; an image's delete is answered with an operation. An instance whose source names a `server` pulls its image
//! into the store its project reads, as a cached image, and a local alias is not checked. A project that holds an image is not
//! empty, so its delete is refused, as Incus refuses it. [`FakeIncus::add_image`] puts an image in a store, as a copy by hand would. A restricted project refuses an instance whose NIC names a network that its `restricted.networks.access` does not list.
//!
//! Writes: a POST to a collection creates an object, a PATCH or DELETE changes or removes one. These cover projects and the
//! eight object kinds (instances, networks, ACLs, profiles, volumes, zones, forwards and buckets). An instance gets a uuid, in
//! its `volatile.uuid` key; no other kind has one, a volume included, as on Incus 7.3. Each object's ETag is a digest of its JSON. A PATCH whose
//! `If-Match` is not the current ETag is 412 and changes nothing. A DELETE ignores `If-Match`, as Incus does. An instance's or
//! a volume's create and delete are answered 202 with an operation, whose URL the caller waits on. Every other write, and
//! every PATCH, is answered sync: 201 for a create and 200 otherwise. A failed operation waits as a status error. A method a
//! route does not take is 405 with an `Allow` header.
//!
//! A PATCH merges the `config` of every kind but a project. A project's config is written as Incus 7.3 writes it
//! (`projectPatch` and `projectPut` in `cmd/incusd/api_project.go`): a PATCH that names `config` replaces the whole config with
//! the keys it sent, and a PUT replaces the config and the description, under `If-Match` like a PATCH. A project and an
//! instance are the objects that take a PUT: an instance's PUT replaces its writable fields whole, under `If-Match`, as an
//! operation. A project that holds an object is in use, and a PATCH or a PUT that would turn off one of its
//! features is refused with 400 and changes nothing, as Incus refuses it.
//!
//! Instances for adoption: an instance that names its `profiles` has its `expanded_devices` and `expanded_config` worked out
//! from those profiles and its own keys, as Incus expands them. A project reads its own profiles when its config sets
//! `features.profiles` to `"true"`, and the default project's otherwise; its `default` profile is empty until a test adds one
//! ([`FakeIncus::add_profile`]). `/1.0/instances/<i>/state` serves the status and each NIC's MAC and IPv4, and a PUT there starts
//! or stops the instance as an operation. An instance create with `start: true` starts it too. A graceful stop can be made to
//! outlast its timeout ([`FakeIncus::hang_stop`]), and then only `force: true` stops it. A NIC's MAC is its device's `hwaddr`,
//! else its `volatile.<dev>.hwaddr`, else one the start
//! generates and keeps there. Its IPv4 is its device's `ipv4.address`, else the lease its MAC holds in the DHCP lease table,
//! else a fresh one the start leases ([`FakeIncus::set_lease`]; `/1.0/networks/<n>/leases`). A lease a start pins, or one
//! [`FakeIncus::set_static_lease`] gives, is listed as static, as Incus lists the lease a NIC's `ipv4.address` names; any other
//! as dynamic. Snapshots and backups are
//! served under `/1.0/instances/<i>/snapshots` and `/1.0/instances/<i>/backups`. A POST to an instance moves it to the project
//! its body names: it must be stopped, the target project must exist, and, as on Incus 7.5, the move is a copy that gets a new
//! `volatile.uuid`. When a PATCH or a move takes a device out of the expanded devices, or changes it in more than its addresses, the fake clears that device's `volatile.<dev>.*` keys as Incus's `deviceVolatileReset`
//! does: every one when the device goes or changes type, and otherwise each one whose key the new device sets. A restricted
//! project refuses a snapshot unless `restricted.snapshots` is `allow`, and a backup unless `restricted.backups` is `allow`.
//! A moved-in instance is refused when a NIC names an ACL that the project serving its network lacks, and, in a restricted
//! project, by the restrictions as Incus 7.5's `checkRestrictions` applies them: the NIC grant, host-path disks and their
//! paths, the device types that need `allow`, privilege, nesting, interception and low-level keys.
//!
//! The fake records each request, and each write with the names of the config keys it carried. It never records a value.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::net::IpAddr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arca_kernel::canon::{Digest, canonical_json};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::shim::{IO_TIMEOUT, drain, head_end, parse_request_line, respond};

/// The project a request names when its query has none, as Incus does.
const DEFAULT_PROJECT: &str = "default";
/// The largest request body the fake reads. A larger one is answered 400.
const MAX_REQUEST_BODY: usize = 1024 * 1024;
/// The largest request head the fake reads.
const MAX_REQUEST_HEAD: usize = 64 * 1024;
/// The longest the fake holds a wait on a running operation, whatever `timeout` the wait names.
const MAX_WAIT_HOLD: Duration = Duration::from_millis(250);
/// The pause between two looks at an operation a wait is held on.
const WAIT_POLL: Duration = Duration::from_millis(5);

/// The metadata the fake serves. The pools are `(name, resources)` pairs.
#[derive(Debug, Clone)]
pub struct FakeState {
    pub server: Value,
    pub projects: Vec<Value>,
    pub instances: Vec<Value>,
    pub pools: Vec<(String, Value)>,
    /// The networks Incus holds. Each names its project, which is `default` for a bridge the host keeps there.
    pub networks: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    server: Value,
    projects: Vec<Value>,
    instances: Vec<Value>,
    pools: Vec<PoolFile>,
    #[serde(default)]
    networks: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolFile {
    name: String,
    resources: Value,
}

impl FakeState {
    /// Reads a state file: `server`, `projects`, `instances`, `pools` as `{name, resources}` objects, and the optional `networks`.
    pub fn load(path: &Path) -> io::Result<FakeState> {
        let bytes = fs::read(path)?;
        let file: StateFile = serde_json::from_slice(&bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the fake state is not in the expected shape",
            )
        })?;
        Ok(FakeState {
            server: file.server,
            projects: file.projects,
            instances: file.instances,
            pools: file
                .pools
                .into_iter()
                .map(|pool| (pool.name, pool.resources))
                .collect(),
            networks: file.networks,
        })
    }
}

/// One request the fake received: its method and target as sent, and the status it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub status: u16,
}

/// One write the fake received: its method, its target as sent, the status it answered and the names of the config keys it
/// carried. A value is never recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRecord {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub config_keys: Vec<String>,
}

/// One host interface: its name and the addresses it holds.
type HostInterface = (String, Vec<IpAddr>);

/// The id of an operation the fake started for a test, in the shape of a uuid.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(pub String);

impl OpId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// When the object of a held create shows: at once, while its operation runs, or only once the operation is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appears {
    WhileRunning,
    OnCompletion,
}

/// A running fake Incus. It serves until it is dropped.
pub struct FakeIncus {
    socket: PathBuf,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

/// What every connection shares: the objects, the logs and the switches.
struct Shared {
    service: Mutex<ServiceFake>,
    store: Mutex<Store>,
    requests: Mutex<Vec<RecordedRequest>>,
    writes: Mutex<Vec<WriteRecord>>,
    reject_config: Mutex<Vec<String>>,
    reject_patch: Mutex<Vec<String>>,
    reject_delete: Mutex<Vec<String>>,
    fail_create_start: AtomicBool,
    host_interfaces: Mutex<Vec<HostInterface>>,
    operations_unreadable: AtomicBool,
    /// The operations whose wait answers 404 while the listing still shows them running.
    waits_ended: Mutex<Vec<String>>,
    console: Mutex<BTreeMap<(String, String), String>>,
    counter: AtomicU64,
    /// The `/1.0/resources` document the fake serves, when a test sets one.
    resources: Mutex<Option<Value>>,
}

/// The served state: the server document, the pools, every object and every operation.
struct Store {
    server: Value,
    pools: Vec<(String, Value)>,
    objects: Vec<Stored>,
    operations: BTreeMap<String, Value>,
    /// What each held operation does when it ends, by its id.
    held: BTreeMap<String, Held>,
    /// Creates a test armed with [`FakeIncus::hold_create`] that have not arrived yet.
    armed: Vec<Armed>,
    /// The DHCP lease table, one lease per MAC.
    leases: Vec<Lease>,
    /// The instances whose graceful stop outlasts its timeout, by name, and whether a forced stop fails too.
    hung: BTreeMap<String, bool>,
    /// The CPU time (nanoseconds) and memory (bytes) a running instance reports, by project and name.
    usage: BTreeMap<(String, String), (u64, u64)>,
}

/// One DHCP lease: the network, the MAC that holds it and its address, and whether a device's `ipv4.address` pinned it.
#[derive(Debug, Clone)]
struct Lease {
    network: String,
    hwaddr: String,
    address: String,
    fixed: bool,
}

/// What a held operation does when it ends: an object that lands on success, and an object that goes on failure.
#[derive(Default)]
struct Held {
    lands: Option<Stored>,
    reverts: Option<Place>,
}

/// An object's place: its kind, its project, its parent and its name, as [`Stored`] keeps them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Place {
    kind: String,
    project: String,
    parent: String,
    name: String,
}

/// A create that will run as a held operation with the id the test was given.
struct Armed {
    id: String,
    place: Place,
    appears: Appears,
}

/// One object: its kind, its project, its name, its parent (the storage pool for a volume or a bucket, the network for a
/// forward, empty otherwise) and its metadata. A project's project is its own name.
#[derive(Debug, Clone)]
struct Stored {
    kind: String,
    project: String,
    name: String,
    parent: String,
    body: Value,
}

/// An object address as a route names it.
#[derive(Debug, Clone)]
struct Address {
    kind: &'static str,
    project: String,
    parent: String,
    name: String,
}

enum Route {
    Server,
    Resources,
    ListProjects,
    ListImages(String),
    ListInstances,
    ListInstancesWithState,
    ProjectInstancesWithState(String),
    ListPools,
    HostInterfaces,
    HostInterfaceState(String),
    PoolResources(String),
    NetworkLeases(String),
    InstanceState(Address),
    Console(Address),
    InstanceSnapshots(Address),
    InstanceBackups(Address),
    ListOperations,
    Operation(String),
    OperationWait(String),
    Collection(Address),
    Object(Address),
}

impl Route {
    fn methods(&self) -> &'static [&'static str] {
        match self {
            Route::Collection(_) => &["POST"],
            Route::Object(address) if address.kind == "project" => {
                &["GET", "PUT", "PATCH", "DELETE"]
            }
            Route::Object(address) if address.kind == "instance" => {
                &["GET", "POST", "PUT", "PATCH", "DELETE"]
            }
            Route::Object(_) => &["GET", "PATCH", "DELETE"],
            Route::InstanceState(_) => &["GET", "PUT"],
            Route::Console(_) => &["GET"],
            Route::InstanceSnapshots(_) => &["GET", "POST"],
            Route::InstanceBackups(_) => &["GET", "POST"],
            _ => &["GET"],
        }
    }
}

impl Shared {
    fn new(state: FakeState) -> Shared {
        let mut objects = Vec::new();
        for body in state.projects {
            let name = text_of(&body, "name");
            objects.push(Stored {
                kind: "project".to_string(),
                project: name.clone(),
                name,
                parent: String::new(),
                body,
            });
        }
        for body in state.instances {
            let project = text_of(&body, "project");
            let name = text_of(&body, "name");
            objects.push(Stored {
                kind: "instance".to_string(),
                project,
                name,
                parent: String::new(),
                body,
            });
        }
        for body in state.networks {
            let project = match text_of(&body, "project") {
                project if project.is_empty() => DEFAULT_PROJECT.to_string(),
                project => project,
            };
            let name = text_of(&body, "name");
            objects.push(Stored {
                kind: "network".to_string(),
                project,
                name,
                parent: String::new(),
                body,
            });
        }
        let mut store = Store {
            server: state.server,
            pools: state.pools,
            objects,
            operations: BTreeMap::new(),
            held: BTreeMap::new(),
            armed: Vec::new(),
            leases: Vec::new(),
            hung: BTreeMap::new(),
            usage: BTreeMap::new(),
        };
        for index in 0..store.objects.len() {
            store.expand(index, false);
        }
        Shared {
            service: Mutex::new(ServiceFake::default()),
            store: Mutex::new(store),
            requests: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            reject_config: Mutex::new(Vec::new()),
            reject_patch: Mutex::new(Vec::new()),
            reject_delete: Mutex::new(Vec::new()),
            fail_create_start: AtomicBool::new(false),
            host_interfaces: Mutex::new(Vec::new()),
            operations_unreadable: AtomicBool::new(false),
            waits_ended: Mutex::new(Vec::new()),
            console: Mutex::new(BTreeMap::new()),
            counter: AtomicU64::new(1),
            resources: Mutex::new(None),
        }
    }

    /// A fresh id in the shape of a uuid, used for an operation and for an object's uuid.
    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        format!("00000000-0000-4000-8000-{n:012x}")
    }
}

impl FakeIncus {
    /// Binds `incus.sock` under `dir` and serves `state` there.
    pub fn start(dir: &Path, state: FakeState) -> io::Result<FakeIncus> {
        let socket = dir.join("incus.sock");
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            thread::spawn(move || accept_loop(&listener, &shared, &stop))
        };
        Ok(FakeIncus {
            socket,
            shared,
            stop,
            accept: Some(accept),
        })
    }

    /// The socket the fake serves on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.shared.requests).clone()
    }

    /// Sets the raw console log a project's instance serves from `/console`.
    pub fn set_console_log(&self, project: &str, name: &str, text: &str) {
        lock(&self.shared.console)
            .insert((project.to_string(), name.to_string()), text.to_string());
    }

    /// Every write received so far, in arrival order: each non-GET request, with the config keys it carried.
    pub fn writes(&self) -> Vec<WriteRecord> {
        lock(&self.shared.writes).clone()
    }

    /// Serves `document` at `/1.0/resources`. Until a test sets one, the fake answers 404, as an Incus without the endpoint.
    pub fn set_resources(&self, document: Value) {
        *lock(&self.shared.resources) = Some(document);
    }

    /// Sets the host's own interfaces: each name with the addresses it holds. The fake answers the listing of
    /// `/1.0/networks?recursion=1` and each `/1.0/networks/<name>/state` from them, and nothing else reads them.
    pub fn set_host_interfaces(&self, interfaces: Vec<(String, Vec<IpAddr>)>) {
        *lock(&self.shared.host_interfaces) = interfaces;
    }

    /// Puts an image with the alias `alias` in the store `project` reads, as a copy by hand would, and returns its fingerprint.
    pub fn add_image(&self, project: &str, alias: &str) -> String {
        let fingerprint =
            Digest::of(format!("{}|{alias}", self.shared.next_id()).as_bytes()).to_hex();
        lock(&self.shared.store).put_image(project, fingerprint, alias, false)
    }

    /// Every image the fake holds, as the project whose store holds it and its fingerprint, in the order they were made.
    pub fn images(&self) -> Vec<(String, String)> {
        lock(&self.shared.store)
            .objects
            .iter()
            .filter(|object| object.kind == "image")
            .map(|object| (object.project.clone(), object.name.clone()))
            .collect()
    }

    /// Makes the fake answer 400 to a POST whose path starts with `kind_path_prefix` and whose body carries a `config`. The
    /// object is not created. A switch for the live probe's rejection case.
    pub fn set_reject_config_on_post(&self, kind_path_prefix: &str) {
        lock(&self.shared.reject_config).push(kind_path_prefix.to_string());
    }

    /// Fail the start phase of create, leaving the new instance stopped.
    pub fn fail_instance_start_on_create(&self) {
        self.shared.fail_create_start.store(true, Ordering::SeqCst);
    }

    /// Makes PATCH requests matching this path prefix fail with 400 before changing any object.
    /// Makes a DELETE of an object whose name starts with `prefix` answer 400.
    pub fn set_reject_delete(&self, prefix: &str) {
        lock(&self.shared.reject_delete).push(prefix.to_string());
    }

    pub fn set_reject_patch(&self, path_prefix: &str) {
        lock(&self.shared.reject_patch).push(path_prefix.to_string());
    }

    /// Starts an operation that names `target` and runs until the test releases or fails it. It changes no object. It shows in
    /// the operations listing at once, and a wait on it answers that it is still running.
    pub fn hold_operation(&self, target: &ObjectRef) -> OpId {
        self.hold_operation_as(target, "task", "Held by the test")
    }

    /// [`FakeIncus::hold_operation`] with the `class` and the `description` Incus gives an operation of some type, such as
    /// `websocket` and `Executing command` for an interactive exec.
    pub fn hold_operation_as(&self, target: &ObjectRef, class: &str, description: &str) -> OpId {
        let id = self.shared.next_id();
        let mut operation = running_operation(&id, &place_of(target));
        operation["class"] = json!(class);
        operation["description"] = json!(description);
        let mut store = lock(&self.shared.store);
        store.operations.insert(id.clone(), operation);
        store.held.insert(id.clone(), Held::default());
        OpId(id)
    }

    /// Makes every wait on operation `id` answer 404 at once, as for an operation the server no longer holds, while the listing
    /// still shows it as it is.
    pub fn answer_wait_ended(&self, id: &OpId) {
        lock(&self.shared.waits_ended).push(id.0.clone());
    }

    /// Makes the next create of `target` run as a held operation with the id returned here, whatever the kind. The create is
    /// answered 202 with the operation, which stays running until the test releases or fails it, and which is not listed before
    /// the create arrives. With [`Appears::WhileRunning`] the object shows at once and goes again if the operation fails, as
    /// Incus reverts a failed create. With [`Appears::OnCompletion`] it shows only when the operation is released.
    pub fn hold_create(&self, target: &ObjectRef, appears: Appears) -> OpId {
        let id = self.shared.next_id();
        lock(&self.shared.store).armed.push(Armed {
            id: id.clone(),
            place: place_of(target),
            appears,
        });
        OpId(id)
    }

    /// Ends a held operation with success. A create held until completion lands now. An id whose create has not arrived yet is
    /// disarmed, and that create then runs as any other does.
    pub fn release_operation(&self, id: &OpId) {
        self.end_operation(id, None);
    }

    /// Ends a held operation with failure, with `message` as its `err`. A held create's object does not land, and one that
    /// showed while the operation ran is gone. An id whose create has not arrived yet is disarmed.
    pub fn fail_operation(&self, id: &OpId, message: &str) {
        self.end_operation(id, Some(message));
    }

    /// Makes the operations listing answer 500 while `unreadable` is true.
    pub fn set_operations_unreadable(&self, unreadable: bool) {
        self.shared
            .operations_unreadable
            .store(unreadable, Ordering::SeqCst);
    }

    /// Puts a profile in `project`, as a create by hand would, and expands again every instance that reads it. It records no
    /// write.
    pub fn add_profile(&self, project: &str, body: Value) {
        let mut store = lock(&self.shared.store);
        let name = text_of(&body, "name");
        store.objects.retain(|object| {
            !(object.kind == "profile" && object.project == project && object.name == name)
        });
        store.objects.push(Stored {
            kind: "profile".to_string(),
            project: project.to_string(),
            name,
            parent: String::new(),
            body,
        });
        for index in 0..store.objects.len() {
            store.expand(index, false);
        }
    }

    /// Gives `mac` the lease of `address` on `network`, replacing any lease the MAC held.
    pub fn set_lease(&self, network: &str, mac: &str, address: &str) {
        lock(&self.shared.store).lease(network, mac, address, false);
    }

    /// Gives `mac` the static lease of `address` on `network`, as Incus lists one for a NIC whose `ipv4.address` names the
    /// address, replacing any lease the MAC held. It shows as `"type": "static"` in the network's lease listing.
    pub fn set_static_lease(&self, network: &str, mac: &str, address: &str) {
        lock(&self.shared.store).lease(network, mac, address, true);
    }

    /// Every lease, as its network, its MAC and its address, in the order they were made.
    pub fn leases(&self) -> Vec<(String, String, String)> {
        lock(&self.shared.store)
            .leases
            .iter()
            .map(|lease| {
                (
                    lease.network.clone(),
                    lease.hwaddr.clone(),
                    lease.address.clone(),
                )
            })
            .collect()
    }

    /// Makes every graceful stop of the instance `name` outlast its timeout: its operation fails and the instance keeps
    /// running. A forced stop then stops it, unless `forced_too` is true, when it fails as well.
    pub fn hang_stop(&self, name: &str, forced_too: bool) {
        lock(&self.shared.store)
            .hung
            .insert(name.to_string(), forced_too);
    }

    /// Sets the CPU time, in nanoseconds, and the memory, in bytes, that the instance `name` in `project` reports in its `state`
    /// on the `recursion=2` listing. Panics when no such instance is held. It records no write.
    pub fn set_instance_usage(&self, project: &str, name: &str, cpu_ns: u64, memory_bytes: u64) {
        let mut store = lock(&self.shared.store);
        assert!(
            store.instance_held(project, name),
            "no instance {project}/{name} to set the usage of"
        );
        store.usage.insert(
            (project.to_string(), name.to_string()),
            (cpu_ns, memory_bytes),
        );
    }

    /// Sets the `last_used_at` of the instance `name` in `project`, as a start does. Panics when no such instance is held. It
    /// records no write.
    pub fn set_last_used(&self, project: &str, name: &str, rfc3339: &str) {
        let mut store = lock(&self.shared.store);
        let object = store
            .objects
            .iter_mut()
            .find(|object| {
                object.kind == "instance" && object.project == project && object.name == name
            })
            .unwrap_or_else(|| panic!("no instance {project}/{name} to set last_used_at of"));
        if let Some(map) = object.body.as_object_mut() {
            map.insert("last_used_at".to_string(), json!(rfc3339));
        }
    }

    fn end_operation(&self, id: &OpId, failure: Option<&str>) {
        let mut guard = lock(&self.shared.store);
        let store: &mut Store = &mut guard;
        store.armed.retain(|armed| armed.id != id.0);
        let Some(held) = store.held.remove(&id.0) else {
            return;
        };
        if let Some(operation) = store.operations.get_mut(&id.0) {
            let (status, code, err) = match failure {
                None => ("Success", 200, ""),
                Some(message) => ("Failure", 400, message),
            };
            operation["status"] = json!(status);
            operation["status_code"] = json!(code);
            operation["err"] = json!(err);
        }
        match failure {
            None => {
                if let Some(object) = held.lands {
                    store.objects.push(object);
                }
            }
            Some(_) => {
                if let Some(place) = held.reverts {
                    store
                        .objects
                        .retain(|object| place_of_stored(object) != place);
                }
            }
        }
    }
}

/// The place of an object a test names, as the fake stores it.
fn place_of(target: &ObjectRef) -> Place {
    let kind = match target.kind {
        IncusKind::Project => "project",
        IncusKind::Instance => "instance",
        IncusKind::Network => "network",
        IncusKind::Acl => "acl",
        IncusKind::Profile => "profile",
        IncusKind::Volume => "volume",
        IncusKind::Zone => "zone",
        IncusKind::Forward => "forward",
        IncusKind::Bucket => "bucket",
    };
    Place {
        kind: kind.to_string(),
        project: target.project.clone(),
        parent: target.parent.clone().unwrap_or_default(),
        name: target.name.clone(),
    }
}

fn place_of_stored(object: &Stored) -> Place {
    Place {
        kind: object.kind.clone(),
        project: object.project.clone(),
        parent: object.parent.clone(),
        name: object.name.clone(),
    }
}

/// A running operation that names the object at `place`, as Incus renders one: its resource URL is the object's path, with
/// `?project=<p>` unless the project is `default`, under the key Incus gives the kind.
fn running_operation(id: &str, place: &Place) -> Value {
    let name = &place.name;
    let parent = &place.parent;
    let (key, path) = match place.kind.as_str() {
        "project" => ("projects", format!("/1.0/projects/{name}")),
        "instance" => ("instances", format!("/1.0/instances/{name}")),
        "network" => ("networks", format!("/1.0/networks/{name}")),
        "acl" => ("network_acls", format!("/1.0/network-acls/{name}")),
        "profile" => ("profiles", format!("/1.0/profiles/{name}")),
        "zone" => ("network_zones", format!("/1.0/network-zones/{name}")),
        "volume" => (
            "storage_volumes",
            format!("/1.0/storage-pools/{parent}/volumes/custom/{name}"),
        ),
        "bucket" => (
            "storage_buckets",
            format!("/1.0/storage-pools/{parent}/buckets/{name}"),
        ),
        _ => (
            "network_forwards",
            format!("/1.0/networks/{parent}/forwards/{name}"),
        ),
    };
    let url = if place.kind == "project" || place.project == DEFAULT_PROJECT {
        path
    } else {
        format!("{path}?project={}", place.project)
    };
    json!({
        "id": id, "class": "task", "description": "Held by the test", "status": "Running", "status_code": 103,
        "err": "", "resources": { key: [url] }, "metadata": null, "may_cancel": false,
    })
}

impl Drop for FakeIncus {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn accept_loop(listener: &UnixListener, shared: &Arc<Shared>, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let shared = Arc::clone(shared);
                thread::spawn(move || serve(stream, &shared));
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// One request as the fake reads it. A head that does not parse has an empty method and is answered 400.
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn bad() -> Request {
        Request {
            method: String::new(),
            target: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The target without its query string.
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }
}

/// One connection: read the request, route it, record it, answer it.
fn serve(mut stream: UnixStream, shared: &Shared) {
    if stream.set_nonblocking(false).is_err()
        || stream.set_read_timeout(Some(IO_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(IO_TIMEOUT)).is_err()
    {
        return;
    }
    let request = match read_request(&mut stream) {
        Ok(Some(request)) => request,
        Ok(None) => return,
        Err(_) => Request::bad(),
    };
    if let Some((status, body)) = service_response(shared, &request) {
        record(shared, &request, status);
        let _ = respond(&mut stream, status, reason(status), "", &body);
        drain(&mut stream);
        return;
    }
    if let Some(console) = console_body(shared, &request) {
        record(shared, &request, console.0);
        let _ = respond(&mut stream, console.0, reason(console.0), "", &console.1);
        drain(&mut stream);
        return;
    }
    let (status, extra, envelope) = handle(shared, &request);
    record(shared, &request, status);
    let _ = respond(
        &mut stream,
        status,
        reason(status),
        &extra,
        &serde_json::to_vec(&envelope).unwrap_or_default(),
    );
    drain(&mut stream);
}

/// Reads one request: its head through the blank line, then a body of `Content-Length` bytes. A chunked request is refused.
fn read_request(stream: &mut UnixStream) -> io::Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_len = loop {
        if let Some(end) = head_end(&buffer) {
            break end;
        }
        if buffer.len() >= MAX_REQUEST_HEAD {
            return Err(invalid("the request head is too large"));
        }
        match stream.read(&mut chunk) {
            Ok(0) if buffer.is_empty() => return Ok(None),
            Ok(0) => return Err(invalid("the request ended before its head")),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) && buffer.is_empty() =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
    };
    let head =
        std::str::from_utf8(&buffer[..head_len]).map_err(|_| invalid("the head is not text"))?;
    let mut lines = head
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line));
    let (method, target) =
        parse_request_line(lines.next().unwrap_or("")).ok_or_else(|| invalid("no request line"))?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("a header without a colon"))?;
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }
    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        return Err(invalid("a chunked request is not read"));
    }
    let length = match headers.iter().find(|(name, _)| name == "content-length") {
        Some((_, value)) => value
            .parse::<usize>()
            .map_err(|_| invalid("a content length that is not a number"))?,
        None => 0,
    };
    if length > MAX_REQUEST_BODY {
        return Err(invalid("the request body is too large"));
    }
    let mut body = buffer[head_len..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(invalid("the request ended before its body"));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(Some(Request {
        method: method.to_string(),
        target: target.to_string(),
        headers,
        body,
    }))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn record(shared: &Shared, request: &Request, status: u16) {
    if !request.path().ends_with("/exec")
        && !request.path().ends_with("/files")
        && !request.path().contains("/logs/exec-output/")
    {
        lock(&shared.service).calls.push(ServiceRequest {
            method: request.method.clone(),
            path: request.target.clone(),
            headers: request.headers.clone(),
            body: request.body.clone(),
        });
    }
    lock(&shared.requests).push(RecordedRequest {
        method: request.method.clone(),
        path: request.target.clone(),
        status,
    });
    if !request.method.is_empty() && request.method != "GET" {
        lock(&shared.writes).push(WriteRecord {
            method: request.method.clone(),
            path: request.target.clone(),
            status,
            config_keys: config_keys(&request.body),
        });
    }
}

/// The names of the keys in a body's `config` object. Values never leave this function.
fn config_keys(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("config")
                .and_then(Value::as_object)
                .map(|config| config.keys().cloned().collect())
        })
        .unwrap_or_default()
}

/// A console-log request's raw `(status, body)`, or `None` for every other route.
fn console_body(shared: &Shared, request: &Request) -> Option<(u16, Vec<u8>)> {
    let Some(Route::Console(address)) = parse(&request.target) else {
        return None;
    };
    if request.method != "GET" {
        return Some((405, Vec::new()));
    }
    let body = lock(&shared.console)
        .get(&(address.project, address.name))
        .cloned();
    match body {
        Some(text) => Some((200, text.into_bytes())),
        None => Some((404, Vec::new())),
    }
}

/// The status, the extra header lines and the envelope for one request.
fn handle(shared: &Shared, request: &Request) -> (u16, String, Value) {
    if request.method.is_empty() {
        return (400, String::new(), error_envelope(400, "bad request"));
    }
    let route = parse(&request.target);
    let allowed: &[&str] = route.as_ref().map_or(&["GET"][..], Route::methods);
    let method = request.method.as_str();
    if let (Some(Route::OperationWait(id)), "GET") = (&route, method) {
        if lock(&shared.waits_ended).contains(id) {
            return (404, String::new(), error_envelope(404, "not found"));
        }
        hold_wait(shared, id, &request.target);
    }
    match route {
        Some(route) if allowed.contains(&method) => dispatch(shared, request, route),
        _ if method == "GET" => (404, String::new(), error_envelope(404, "not found")),
        _ => (
            405,
            format!("Allow: {}\r\n", allowed.join(", ")),
            error_envelope(405, "method not allowed"),
        ),
    }
}

/// Holds a wait while its operation runs, for the `timeout` the wait names, at most [`MAX_WAIT_HOLD`]. The store is not locked
/// while the wait is held, so the test can end the operation meanwhile.
fn hold_wait(shared: &Shared, id: &str, target: &str) {
    let timeout = target
        .split_once('?')
        .and_then(|(_, query)| query_param(query, "timeout"))
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(MAX_WAIT_HOLD, |seconds| {
            Duration::from_secs(seconds).min(MAX_WAIT_HOLD)
        });
    let started = Instant::now();
    loop {
        let running = lock(&shared.store)
            .operations
            .get(id)
            .and_then(|operation| operation.get("status_code"))
            .and_then(Value::as_u64)
            .is_some_and(|code| (100..200).contains(&code));
        if !running || started.elapsed() >= timeout {
            return;
        }
        thread::sleep(WAIT_POLL);
    }
}

/// The route for a target, or `None` when no route serves it.
fn parse(target: &str) -> Option<Route> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let rest = path.strip_prefix("/1.0")?;
    let segments: Vec<&str> = if rest.is_empty() {
        Vec::new()
    } else {
        rest.strip_prefix('/')?.split('/').collect()
    };
    let project = query_param(query, "project").unwrap_or(DEFAULT_PROJECT);
    let address = |kind: &'static str, parent: &str, name: &str| Address {
        kind,
        project: project.to_string(),
        parent: parent.to_string(),
        name: name.to_string(),
    };
    let route = match segments.as_slice() {
        [] if query.is_empty() => Route::Server,
        ["resources"] if query.is_empty() => Route::Resources,
        ["projects"] if query == "recursion=1" => Route::ListProjects,
        ["projects"] => Route::Collection(Address {
            kind: "project",
            project: String::new(),
            parent: String::new(),
            name: String::new(),
        }),
        ["projects", name] => Route::Object(Address {
            kind: "project",
            project: (*name).to_string(),
            parent: String::new(),
            name: (*name).to_string(),
        }),
        ["instances"] if query == "all-projects=true&recursion=1" => Route::ListInstances,
        ["instances"] if query == "all-projects=true&recursion=2" => Route::ListInstancesWithState,
        ["instances"] if query_param(query, "recursion") == Some("2") => {
            Route::ProjectInstancesWithState(project.into())
        }
        ["instances"] => Route::Collection(address("instance", "", "")),
        ["instances", name] => Route::Object(address("instance", "", name)),
        ["instances", name, "state"] => Route::InstanceState(address("instance", "", name)),
        ["instances", name, "console"] => Route::Console(address("instance", "", name)),
        ["instances", name, "snapshots"] => Route::InstanceSnapshots(address("snapshot", name, "")),
        ["instances", name, "snapshots", snapshot] => {
            Route::Object(address("snapshot", name, snapshot))
        }
        ["instances", name, "backups"] => Route::InstanceBackups(address("backup", name, "")),
        ["networks", name, "leases"] => Route::NetworkLeases((*name).to_string()),
        ["networks"] if query == "recursion=1" => Route::HostInterfaces,
        ["networks"] => Route::Collection(address("network", "", "")),
        ["networks", name, "state"] if query.is_empty() => {
            Route::HostInterfaceState((*name).to_string())
        }
        ["networks", name] => Route::Object(address("network", "", name)),
        ["networks", network, "forwards"] => Route::Collection(address("forward", network, "")),
        ["networks", network, "forwards", name] => Route::Object(address("forward", network, name)),
        ["network-acls"] => Route::Collection(address("acl", "", "")),
        ["network-acls", name] => Route::Object(address("acl", "", name)),
        ["profiles"] => Route::Collection(address("profile", "", "")),
        ["profiles", name] => Route::Object(address("profile", "", name)),
        ["network-zones"] => Route::Collection(address("zone", "", "")),
        ["network-zones", name] => Route::Object(address("zone", "", name)),
        ["images"] if query_param(query, "recursion") == Some("1") => {
            Route::ListImages(project.to_string())
        }
        ["images", fingerprint] => Route::Object(address("image", "", fingerprint)),
        ["storage-pools"] if query == "recursion=1" => Route::ListPools,
        ["storage-pools", pool, "resources"] if query.is_empty() => {
            Route::PoolResources((*pool).to_string())
        }
        ["storage-pools", pool, "volumes", "custom"] => {
            Route::Collection(address("volume", pool, ""))
        }
        ["storage-pools", pool, "volumes", "custom", name] => {
            Route::Object(address("volume", pool, name))
        }
        ["storage-pools", pool, "buckets"] => Route::Collection(address("bucket", pool, "")),
        ["storage-pools", pool, "buckets", name] => Route::Object(address("bucket", pool, name)),
        ["operations"] if query == "all-projects=true&recursion=1" => Route::ListOperations,
        ["operations", id] => Route::Operation((*id).to_string()),
        ["operations", id, "wait"] => Route::OperationWait((*id).to_string()),
        _ => return None,
    };
    Some(route)
}

/// The value of `key` in a query string, when it is present.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
}

fn dispatch(shared: &Shared, request: &Request, route: Route) -> (u16, String, Value) {
    let mut guard = lock(&shared.store);
    let store: &mut Store = &mut guard;
    let route = match route {
        Route::Collection(address) => Route::Collection(store.served_from(address)),
        Route::Object(address) => Route::Object(store.served_from(address)),
        other => other,
    };
    match (request.method.as_str(), route) {
        ("GET", Route::Server) => sync(store.server.clone()),
        ("GET", Route::Resources) => match lock(&shared.resources).clone() {
            Some(document) => sync(document),
            None => (404, String::new(), error_envelope(404, "not found")),
        },
        ("GET", Route::ListProjects) => sync(Value::Array(store.bodies("project"))),
        ("GET", Route::ListImages(project)) => match store.image_store(&project) {
            Some(home) => sync(Value::Array(
                store
                    .objects
                    .iter()
                    .filter(|object| object.kind == "image" && object.project == home)
                    .map(|object| object.body.clone())
                    .collect(),
            )),
            None => (
                404,
                String::new(),
                error_envelope(404, "the project is not found"),
            ),
        },
        ("GET", Route::ListInstances) => sync(Value::Array(store.bodies("instance"))),
        ("GET", Route::ListInstancesWithState) => sync(Value::Array(store.instances_with_state())),
        ("GET", Route::ProjectInstancesWithState(project)) => {
            if project != "default"
                && !store
                    .objects
                    .iter()
                    .any(|object| object.kind == "project" && object.name == project)
            {
                (
                    404,
                    String::new(),
                    error_envelope(404, "the project is not found"),
                )
            } else {
                sync(Value::Array(
                    store
                        .instances_with_state()
                        .into_iter()
                        .filter(|row| row["project"] == project)
                        .collect(),
                ))
            }
        }
        ("GET", Route::HostInterfaces) => {
            let interfaces = lock(&shared.host_interfaces);
            sync(Value::Array(
                interfaces
                    .iter()
                    .map(|(name, _)| json!({ "name": name, "managed": false, "type": "broadcast" }))
                    .collect(),
            ))
        }
        ("GET", Route::HostInterfaceState(name)) => {
            let interfaces = lock(&shared.host_interfaces);
            match interfaces.iter().find(|(listed, _)| *listed == name) {
                Some((_, addresses)) => sync(json!({
                    "addresses": addresses
                        .iter()
                        .map(|address| json!({
                            "family": if address.is_ipv4() { "inet" } else { "inet6" },
                            "address": address.to_string(),
                        }))
                        .collect::<Vec<Value>>(),
                })),
                None => (404, String::new(), error_envelope(404, "not found")),
            }
        }
        ("GET", Route::ListPools) => sync(Value::Array(
            store
                .pools
                .iter()
                .map(|(name, _)| json!({ "name": name }))
                .collect(),
        )),
        ("GET", Route::PoolResources(pool)) => {
            match store.pools.iter().find(|(name, _)| *name == pool) {
                Some((_, resources)) => sync(resources.clone()),
                None => (404, String::new(), error_envelope(404, "not found")),
            }
        }
        ("GET", Route::ListOperations) => {
            if shared.operations_unreadable.load(Ordering::SeqCst) {
                return (
                    500,
                    String::new(),
                    error_envelope(500, "the operations cannot be listed"),
                );
            }
            let mut groups = Map::new();
            for operation in store.operations.values() {
                let status = operation
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if let Value::Array(list) = groups.entry(status).or_insert_with(|| json!([])) {
                    list.push(operation.clone());
                }
            }
            sync(Value::Object(groups))
        }
        ("GET", Route::Operation(id) | Route::OperationWait(id)) => match store.operations.get(&id)
        {
            Some(operation) => sync(operation.clone()),
            None => (404, String::new(), error_envelope(404, "not found")),
        },
        ("GET", Route::Object(address)) => match store.find(&address) {
            Some(index) => {
                let body = store.objects[index].body.clone();
                let extra = format!("ETag: {}\r\n", etag_of(&body));
                (200, extra, sync_envelope(body))
            }
            None => (404, String::new(), error_envelope(404, "not found")),
        },
        ("GET", Route::NetworkLeases(network)) => sync(Value::Array(
            store
                .leases
                .iter()
                .filter(|lease| lease.network == network)
                .map(|lease| {
                    json!({
                        "hwaddr": lease.hwaddr, "address": lease.address,
                        "type": if lease.fixed { "static" } else { "dynamic" },
                    })
                })
                .collect(),
        )),
        ("GET", Route::InstanceState(address)) => instance_state(store, &address),
        ("PUT", Route::InstanceState(address)) => change_state(shared, store, request, &address),
        ("GET", Route::InstanceSnapshots(address) | Route::InstanceBackups(address)) => {
            sync(Value::Array(
                store
                    .objects
                    .iter()
                    .filter(|object| {
                        object.kind == address.kind
                            && object.project == address.project
                            && object.parent == address.parent
                    })
                    .map(|object| object.body.clone())
                    .collect(),
            ))
        }
        ("POST", Route::InstanceSnapshots(address)) => {
            snapshot_or_backup(shared, store, request, &address, "snapshot")
        }
        ("POST", Route::InstanceBackups(address)) => {
            snapshot_or_backup(shared, store, request, &address, "backup")
        }
        ("POST", Route::Object(address)) => move_instance(shared, store, request, &address),
        ("POST", Route::Collection(address)) => create(shared, store, request, &address),
        ("PATCH", Route::Object(address)) => patch(shared, store, request, &address),
        ("PUT", Route::Object(address)) => put(shared, store, request, &address),
        ("DELETE", Route::Object(address)) => delete(shared, store, &address),
        _ => (
            405,
            String::new(),
            error_envelope(405, "method not allowed"),
        ),
    }
}

impl Store {
    /// The address a network, a forward or an ACL is served at. Incus serves a project that does not enable `features.networks`
    /// the networks and the ACLs of the default project, so such an address is moved there. An address whose project does not exist is kept, and
    /// answers as Incus answers a missing project.
    fn served_from(&self, mut address: Address) -> Address {
        if address.kind == "image" {
            if let Some(home) = self.image_store(&address.project) {
                address.project = home;
            }
            return address;
        }
        if !matches!(address.kind, "network" | "forward" | "acl")
            || address.project == DEFAULT_PROJECT
        {
            return address;
        }
        let Some(project) = self
            .objects
            .iter()
            .find(|object| object.kind == "project" && object.project == address.project)
        else {
            return address;
        };
        let enables = project
            .body
            .get("config")
            .and_then(|config| config.get("features.networks"))
            .and_then(Value::as_str)
            == Some("true");
        if !enables {
            address.project = DEFAULT_PROJECT.to_string();
        }
        address
    }

    /// The project whose image store `project` reads: its own when its config sets `features.images` to `"true"`, and the default
    /// project's otherwise. `None` when the project does not exist.
    fn image_store(&self, project: &str) -> Option<String> {
        if project == DEFAULT_PROJECT {
            return Some(DEFAULT_PROJECT.to_string());
        }
        let config = self
            .objects
            .iter()
            .find(|object| object.kind == "project" && object.project == project)?
            .body
            .get("config")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let own = config.get("features.images").and_then(Value::as_str) == Some("true");
        Some(if own { project } else { DEFAULT_PROJECT }.to_string())
    }

    /// Puts an image in the store `project` reads, unless one with that fingerprint is there already, and returns the fingerprint.
    fn put_image(
        &mut self,
        project: &str,
        fingerprint: String,
        alias: &str,
        cached: bool,
    ) -> String {
        let home = self
            .image_store(project)
            .unwrap_or_else(|| project.to_string());
        let held = self.objects.iter().any(|object| {
            object.kind == "image" && object.project == home && object.name == fingerprint
        });
        if !held {
            let aliases = if cached {
                json!([])
            } else {
                json!([{ "name": alias }])
            };
            self.objects.push(Stored {
                kind: "image".to_string(),
                project: home.clone(),
                name: fingerprint.clone(),
                parent: String::new(),
                body: json!({
                    "fingerprint": fingerprint, "project": home, "cached": cached,
                    "aliases": aliases, "type": "container",
                }),
            });
        }
        fingerprint
    }

    /// The project's own config, when the project exists.
    fn project_config(&self, name: &str) -> Option<&Map<String, Value>> {
        self.objects
            .iter()
            .find(|object| object.kind == "project" && object.project == name)
            .and_then(|object| object.body.get("config"))
            .and_then(Value::as_object)
    }

    /// The reason an instance body is refused by its project's restriction, when a NIC names a network the project does not
    /// list in `restricted.networks.access`. A project that is not restricted lists nothing and refuses nothing.
    fn restricted_nic(&self, project: &str, body: &Map<String, Value>) -> Option<&'static str> {
        let config = self.project_config(project)?;
        if config.get("restricted").and_then(Value::as_str) != Some("true") {
            return None;
        }
        let granted: Vec<&str> = config
            .get("restricted.networks.access")
            .and_then(Value::as_str)
            .unwrap_or("")
            .split(',')
            .collect();
        let devices = body.get("devices").and_then(Value::as_object)?;
        let refused = devices.values().any(|device| {
            device.get("type").and_then(Value::as_str) == Some("nic")
                && device
                    .get("network")
                    .and_then(Value::as_str)
                    .is_some_and(|network| !granted.contains(&network))
        });
        refused.then_some("the project may not use the network")
    }

    fn find(&self, address: &Address) -> Option<usize> {
        self.objects.iter().position(|object| {
            object.kind == address.kind
                && object.project == address.project
                && object.parent == address.parent
                && object.name == address.name
        })
    }

    fn bodies(&self, kind: &str) -> Vec<Value> {
        self.objects
            .iter()
            .filter(|object| object.kind == kind)
            .map(|object| object.body.clone())
            .collect()
    }

    /// Whether the instance `name` in `project` is held.
    fn instance_held(&self, project: &str, name: &str) -> bool {
        self.objects.iter().any(|object| {
            object.kind == "instance" && object.project == project && object.name == name
        })
    }

    /// Every instance, as the `recursion=2` listing serves it: its body and a `state`. A running or frozen instance's `state` holds its
    /// usage, which is zero until a test sets it; a stopped instance's `state` is an object of zeros, as Incus 7.5.1 answers.
    fn instances_with_state(&self) -> Vec<Value> {
        self.objects
            .iter()
            .filter(|object| object.kind == "instance")
            .map(|object| {
                let mut body = object.body.clone();
                let (cpu_ns, memory) = if text_of(&body, "status") != "Stopped" {
                    self.usage
                        .get(&(object.project.clone(), object.name.clone()))
                        .copied()
                        .unwrap_or((0, 0))
                } else {
                    (0, 0)
                };
                let state = json!({ "cpu": { "usage": cpu_ns }, "memory": { "usage": memory } });
                if let Some(map) = body.as_object_mut() {
                    map.insert("state".to_string(), state);
                }
                body
            })
            .collect()
    }
}

/// Creates an object from a POST. A `config` is refused when the switch names this path.
fn create(
    shared: &Shared,
    store: &mut Store,
    request: &Request,
    address: &Address,
) -> (u16, String, Value) {
    let Ok(Value::Object(mut body)) = serde_json::from_slice::<Value>(&request.body) else {
        return (
            400,
            String::new(),
            error_envelope(400, "the body is not a JSON object"),
        );
    };
    let path = request.path();
    let rejected = body.contains_key("config")
        && lock(&shared.reject_config)
            .iter()
            .any(|prefix| path.starts_with(prefix.as_str()));
    if rejected {
        return (
            400,
            String::new(),
            error_envelope(400, "config is not accepted on create"),
        );
    }
    if body.get("config").is_some_and(|config| !config.is_object()) {
        return (
            400,
            String::new(),
            error_envelope(400, "config must be an object"),
        );
    }
    let kind = address.kind;
    if kind == "acl" {
        for (field, empty) in [
            ("description", json!("")),
            ("ingress", json!([])),
            ("egress", json!([])),
        ] {
            body.entry(field).or_insert(empty);
        }
    }
    let name_key = if kind == "forward" {
        "listen_address"
    } else {
        "name"
    };
    let name = text_in(&body, name_key);
    if name.is_empty() || name.contains('/') {
        return (
            400,
            String::new(),
            error_envelope(400, "the object has no usable name"),
        );
    }
    let project = if kind == "project" {
        name.clone()
    } else {
        address.project.clone()
    };
    if kind == "instance"
        && let Some(reason) = store.restricted_nic(&project, &body)
    {
        return (403, String::new(), error_envelope(403, reason));
    }
    if kind == "instance"
        && let Some(source) = body.get("source").and_then(Value::as_object)
        && let Some(server) = source.get("server").and_then(Value::as_str)
    {
        // The image is pulled from the server into the store the project reads, and kept there as a cached image.
        let alias = source.get("alias").and_then(Value::as_str).unwrap_or("");
        let fingerprint = Digest::of(format!("{server}|{alias}").as_bytes()).to_hex();
        store.put_image(&project, fingerprint, alias, true);
    }
    let parent = address.parent.clone();
    if matches!(kind, "volume" | "bucket") && !store.pools.iter().any(|(pool, _)| *pool == parent) {
        return (
            404,
            String::new(),
            error_envelope(404, "the storage pool is not found"),
        );
    }
    if kind == "forward"
        && store
            .find(&Address {
                kind: "network",
                project: project.clone(),
                parent: String::new(),
                name: parent.clone(),
            })
            .is_none()
    {
        return (
            404,
            String::new(),
            error_envelope(404, "the network is not found"),
        );
    }
    if store.objects.iter().any(|object| {
        object.kind == kind
            && object.project == project
            && object.parent == parent
            && object.name == name
    }) {
        return (
            409,
            String::new(),
            error_envelope(409, "the object already exists"),
        );
    }
    body.insert("name".to_string(), Value::String(name.clone()));
    if kind != "project" {
        body.insert("project".to_string(), Value::String(project.clone()));
    }
    body.entry("config").or_insert_with(|| json!({}));
    if kind == "instance" {
        // Incus gives an instance a uuid, in its `volatile.uuid` config key. No other kind has one: Incus 7.3 gives a custom
        // volume none.
        if let Some(config) = body.get_mut("config").and_then(Value::as_object_mut) {
            config.insert("volatile.uuid".to_string(), Value::String(shared.next_id()));
        }
    }
    match kind {
        "instance" => {
            let status = if body.get("start") == Some(&json!(true)) {
                "Running"
            } else {
                "Stopped"
            };
            body.entry("status").or_insert_with(|| json!(status));
        }
        "volume" => {
            body.insert("pool".to_string(), Value::String(parent.clone()));
            body.entry("type")
                .or_insert_with(|| Value::String("custom".to_string()));
        }
        _ => {}
    }
    let stored = Stored {
        kind: kind.to_string(),
        project,
        name,
        parent,
        body: Value::Object(body),
    };
    let place = place_of_stored(&stored);
    if let Some(at) = store.armed.iter().position(|armed| armed.place == place) {
        let armed = store.armed.remove(at);
        let held = match armed.appears {
            Appears::WhileRunning => {
                store.objects.push(stored);
                Held {
                    lands: None,
                    reverts: Some(place.clone()),
                }
            }
            Appears::OnCompletion => Held {
                lands: Some(stored),
                reverts: None,
            },
        };
        let operation = running_operation(&armed.id, &place);
        store.operations.insert(armed.id.clone(), operation.clone());
        store.held.insert(armed.id.clone(), held);
        return (202, String::new(), async_envelope(&armed.id, operation));
    }
    store.objects.push(stored);
    if kind == "instance" {
        let at = store.objects.len() - 1;
        if store.objects[at].body.get("start") == Some(&json!(true)) {
            if shared.fail_create_start.load(Ordering::SeqCst) {
                store.objects[at].body["status"] = json!("Stopped");
                store.expand(at, false);
                return answered(
                    shared,
                    store,
                    kind,
                    "POST",
                    Err((400, "Failed starting instance".into())),
                );
            }
            store.start(at, shared.counter.fetch_add(1, Ordering::SeqCst));
        } else {
            store.expand(at, false);
        }
    }
    answered(shared, store, kind, "POST", Ok(()))
}

/// The project config keys that are features. Incus refuses a write that turns one off on a project in use.
const PROJECT_FEATURES: [&str; 6] = [
    "features.images",
    "features.networks",
    "features.networks.zones",
    "features.profiles",
    "features.storage.buckets",
    "features.storage.volumes",
];

/// A complete answer: the status, the extra header lines and the envelope.
type Answer = (u16, String, Value);

/// The object a write names and its body, after the `If-Match` check: 404 when it is not there, 412 when `If-Match` is not its
/// current ETag, and 400 when the body is not a JSON object.
fn written(
    store: &Store,
    request: &Request,
    address: &Address,
) -> Result<(usize, Map<String, Value>), Answer> {
    let Some(index) = store.find(address) else {
        return Err((404, String::new(), error_envelope(404, "not found")));
    };
    if let Some(sent) = request.header("if-match")
        && sent != etag_of(&store.objects[index].body)
    {
        return Err((
            412,
            String::new(),
            error_envelope(412, "the object changed after it was read"),
        ));
    }
    let Ok(Value::Object(changes)) = serde_json::from_slice::<Value>(&request.body) else {
        return Err((
            400,
            String::new(),
            error_envelope(400, "the body is not a JSON object"),
        ));
    };
    Ok((index, changes))
}

/// Replaces a project or an instance from a PUT. A project's config and description are the ones sent, and an absent one is
/// empty, as Incus's `projectPut` does. An instance's writable fields (`api.InstancePut`) are the ones sent, an absent one
/// empty, and its devices are expanded again with the volatile reset; it is answered with an operation, as `instancePut` is.
/// Only a project's and an instance's routes take a PUT.
fn put(
    shared: &Shared,
    store: &mut Store,
    request: &Request,
    address: &Address,
) -> (u16, String, Value) {
    let (index, body) = match written(store, request, address) {
        Ok(found) => found,
        Err(answer) => return answer,
    };
    if store.objects[index].kind == "project" {
        return write_project(shared, store, index, &body, true);
    }
    let bad = |message: &str| (400, String::new(), error_envelope(400, message));
    let text_map = |key: &str| match body.get(key) {
        None => Some(Map::new()),
        Some(Value::Object(map)) if map.values().all(Value::is_string) => Some(map.clone()),
        Some(_) => None,
    };
    let Some(config) = text_map("config") else {
        return bad("config must be a map of text");
    };
    let devices = match body.get("devices") {
        None => Map::new(),
        Some(Value::Object(devices)) if devices.values().all(Value::is_object) => devices.clone(),
        Some(_) => return bad("devices must be a map of objects"),
    };
    let profiles = match body.get("profiles") {
        None => json!([]),
        Some(Value::Array(names)) if names.iter().all(Value::is_string) => json!(names),
        Some(_) => return bad("profiles must be a list of names"),
    };
    let Some(object) = store.objects[index].body.as_object_mut() else {
        return (
            500,
            String::new(),
            error_envelope(500, "the object is not an object"),
        );
    };
    object.insert("config".to_string(), Value::Object(config));
    object.insert("devices".to_string(), Value::Object(devices));
    object.insert("profiles".to_string(), profiles);
    for (field, empty) in [
        ("architecture", json!("")),
        ("description", json!("")),
        ("ephemeral", json!(false)),
        ("stateful", json!(false)),
    ] {
        object.insert(field.to_string(), body.get(field).cloned().unwrap_or(empty));
    }
    store.expand(index, true);
    answered(shared, store, "instance", "PUT", Ok(()))
}

/// Changes an object from a PATCH. A stale `If-Match` is 412 and changes nothing. An empty string in `config` removes its key.
/// A project's PATCH replaces its whole config (see [`write_project`]). An ACL's PATCH merges only config;
/// description and both rule arrays are replaced, including empty defaults for omitted fields (Incus v7.5.1/v7.6.0).
fn patch(
    shared: &Shared,
    store: &mut Store,
    request: &Request,
    address: &Address,
) -> (u16, String, Value) {
    if lock(&shared.reject_patch)
        .iter()
        .any(|prefix| request.path().starts_with(prefix))
    {
        return (
            400,
            String::new(),
            error_envelope(400, "patch rejected by test"),
        );
    }
    let (index, changes) = match written(store, request, address) {
        Ok(found) => found,
        Err(answer) => return answer,
    };
    if store.objects[index].kind == "project" {
        return write_project(shared, store, index, &changes, false);
    }
    let mut changes = changes;
    if store.objects[index].kind == "acl" {
        // networkACLPut writes existing config keys into req.Config. With no
        // config map and any existing key, upstream fails before applying it.
        if !changes.get("config").is_some_and(Value::is_object)
            && store.objects[index].body["config"]
                .as_object()
                .is_some_and(|config| !config.is_empty())
        {
            return (
                500,
                String::new(),
                error_envelope(500, "ACL PATCH requires a config map"),
            );
        }
        for (field, empty) in [
            ("description", json!("")),
            ("ingress", json!([])),
            ("egress", json!([])),
        ] {
            changes.entry(field).or_insert(empty);
        }
    }
    let devices = (store.objects[index].kind == "instance")
        .then(|| changes.remove("devices"))
        .flatten();
    let Some(object) = store.objects[index].body.as_object_mut() else {
        return (
            500,
            String::new(),
            error_envelope(500, "the object is not an object"),
        );
    };
    if let Err(message) = merge(object, &changes) {
        return (400, String::new(), error_envelope(400, message));
    }
    if let Some(devices) = devices {
        // An instance's PATCH replaces each device it names, whole, and keeps the others, as Incus's `instancePatch` does.
        let Value::Object(devices) = devices else {
            return (
                400,
                String::new(),
                error_envelope(400, "devices must be an object"),
            );
        };
        let local = object
            .entry("devices")
            .or_insert_with(|| json!({}))
            .as_object_mut();
        let Some(local) = local else {
            return (
                400,
                String::new(),
                error_envelope(400, "devices must be an object"),
            );
        };
        for (name, device) in devices {
            if !device.is_object() {
                return (
                    400,
                    String::new(),
                    error_envelope(400, "a device must be an object"),
                );
            }
            local.insert(name, device);
        }
    }
    let kind = store.objects[index].kind.clone();
    if kind == "instance" {
        store.expand(index, true);
    }
    answered(shared, store, &kind, "PATCH", Ok(()))
}

/// Writes a project's config and description as Incus 7.3 does. A PUT sets both to what it sent, and an absent one is empty.
/// A PATCH that names `config` replaces the whole config with the keys it sent, and keeps the description unless it names one:
/// Incus's `projectPatch` copies the old keys into the raw request map, not into the request it applies, so every key the PATCH
/// did not name is dropped. An empty value is unset. A project in use (one that holds an object) refuses a write that turns off
/// one of its features, with 400, and nothing changes.
fn write_project(
    shared: &Shared,
    store: &mut Store,
    index: usize,
    sent: &Map<String, Value>,
    put: bool,
) -> (u16, String, Value) {
    let bad = |message: &str| (400, String::new(), error_envelope(400, message));
    let current = store.objects[index].body.clone();
    let config = match sent.get("config") {
        Some(Value::Object(config)) => {
            let mut kept = Map::new();
            for (key, value) in config {
                match value {
                    Value::String(text) if text.is_empty() => {}
                    Value::String(_) => {
                        kept.insert(key.clone(), value.clone());
                    }
                    Value::Null => {}
                    _ => return bad("a config value must be text"),
                }
            }
            kept
        }
        Some(_) => return bad("config must be an object"),
        None if put => Map::new(),
        None => current
            .get("config")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
    };
    let description = match sent.get("description") {
        Some(Value::String(text)) => text.clone(),
        Some(_) => return bad("description must be text"),
        None if put => String::new(),
        None => text_of(&current, "description"),
    };
    let name = store.objects[index].name.clone();
    let in_use = store
        .objects
        .iter()
        .any(|object| object.kind != "project" && object.project == name);
    if in_use {
        let was = |key: &str| current.get("config").and_then(|config| config.get(key));
        for feature in PROJECT_FEATURES {
            if was(feature).and_then(Value::as_str) == Some("true")
                && was(feature) != config.get(feature)
            {
                return bad(&format!(
                    "Project feature \"{feature}\" cannot be disabled on non-empty projects"
                ));
            }
        }
    }
    let Some(object) = store.objects[index].body.as_object_mut() else {
        return (
            500,
            String::new(),
            error_envelope(500, "the object is not an object"),
        );
    };
    object.insert("config".to_string(), Value::Object(config));
    object.insert("description".to_string(), Value::String(description));
    let method = if put { "PUT" } else { "PATCH" };
    answered(shared, store, "project", method, Ok(()))
}

/// Applies a PATCH body to an object. Top-level keys replace; `config` keys merge, and an empty or null value removes a key.
fn merge(
    object: &mut Map<String, Value>,
    changes: &Map<String, Value>,
) -> Result<(), &'static str> {
    for (key, value) in changes {
        if key != "config" {
            object.insert(key.clone(), value.clone());
            continue;
        }
        let Some(edits) = value.as_object() else {
            return Err("config must be an object");
        };
        let config = object
            .entry("config")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or("config must be an object")?;
        for (name, edit) in edits {
            match edit {
                Value::Null => {
                    config.remove(name);
                }
                Value::String(text) if text.is_empty() => {
                    config.remove(name);
                }
                Value::String(_) => {
                    config.insert(name.clone(), edit.clone());
                }
                _ => return Err("a config value must be text"),
            }
        }
    }
    Ok(())
}

/// Removes an object. A non-empty project is refused at once. A running instance's delete fails inside its operation.
fn delete(shared: &Shared, store: &mut Store, address: &Address) -> (u16, String, Value) {
    if lock(&shared.reject_delete)
        .iter()
        .any(|prefix| address.name.starts_with(prefix))
    {
        return (
            400,
            String::new(),
            error_envelope(400, "the delete was refused"),
        );
    }
    let Some(index) = store.find(address) else {
        return (404, String::new(), error_envelope(404, "not found"));
    };
    let kind = store.objects[index].kind.clone();
    if kind == "project"
        && store
            .objects
            .iter()
            .any(|object| object.kind != "project" && object.project == address.name)
    {
        return (
            400,
            String::new(),
            error_envelope(400, "the project is not empty"),
        );
    }
    if kind == "instance" && store.objects[index].body.get("status") == Some(&json!("Running")) {
        return answered(
            shared,
            store,
            &kind,
            "DELETE",
            Err((400, "The instance is running".to_string())),
        );
    }
    store.objects.remove(index);
    answered(shared, store, &kind, "DELETE", Ok(()))
}

/// Whether Incus answers a write to this kind and method with an operation. An instance's and a volume's create and delete
/// are, and so is an image's delete. Their config update is sync, as every other write is.
fn is_async(kind: &str, method: &str) -> bool {
    matches!(
        kind,
        "instance" | "volume" | "image" | "snapshot" | "backup"
    ) && method != "PATCH"
}

/// Answers a write. An async write records its operation and answers 202 with the operation's URL; the operation's own status
/// carries the outcome. A sync write answers with its result: 201 for a create, 200 for a change or a delete.
fn answered(
    shared: &Shared,
    store: &mut Store,
    kind: &str,
    method: &str,
    outcome: Result<(), (u16, String)>,
) -> (u16, String, Value) {
    if !is_async(kind, method) {
        return match outcome {
            Ok(()) if method == "POST" => (201, String::new(), sync_envelope(Value::Null)),
            Ok(()) => (200, String::new(), sync_envelope(Value::Null)),
            Err((code, message)) => (code, String::new(), error_envelope(code, &message)),
        };
    }
    let id = shared.next_id();
    let operation = match outcome {
        Ok(()) => json!({
            "id": id, "class": "task", "status": "Success", "status_code": 200,
            "err": "", "resources": {}, "metadata": null,
        }),
        Err((code, message)) => json!({
            "id": id, "class": "task", "status": "Failure", "status_code": code,
            "err": message, "resources": {}, "metadata": null,
        }),
    };
    let envelope = async_envelope(&id, operation.clone());
    store.operations.insert(id, operation);
    (202, String::new(), envelope)
}

/// The envelope of a write Incus accepted as operation `id`.
fn async_envelope(id: &str, operation: Value) -> Value {
    json!({
        "type": "async", "status": "Operation created", "status_code": 100,
        "operation": format!("/1.0/operations/{id}"), "error_code": 0, "error": "", "metadata": operation,
    })
}

/// The ETag of an object: a digest of its canonical JSON, quoted as HTTP has it.
fn etag_of(body: &Value) -> String {
    let text = canonical_json(body).unwrap_or_default();
    format!("\"{}\"", Digest::of(text.as_bytes()).to_hex())
}

fn text_of(body: &Value, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn text_in(body: &Map<String, Value>, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn sync(metadata: Value) -> (u16, String, Value) {
    (200, String::new(), sync_envelope(metadata))
}

fn sync_envelope(metadata: Value) -> Value {
    json!({
        "type": "sync",
        "status": "Success",
        "status_code": 200,
        "operation": "",
        "error_code": 0,
        "error": "",
        "metadata": metadata,
    })
}

fn error_envelope(status: u16, message: &str) -> Value {
    json!({
        "type": "error",
        "status": reason(status),
        "status_code": status,
        "operation": "",
        "error_code": status,
        "error": message,
        "metadata": null,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        412 => "Precondition Failed",
        500 => "Internal Server Error",
        _ => "Bad Request",
    }
}

/// The NIC keys Incus changes in place. A device that changes only in these keeps its volatile keys.
const UPDATABLE_NIC_KEYS: [&str; 2] = ["ipv4.address", "ipv6.address"];

impl Store {
    /// The project whose profiles `project` reads: its own when its config sets `features.profiles` to `"true"`, and the
    /// default project's otherwise.
    fn profile_home(&self, project: &str) -> String {
        let own = self
            .project_config(project)
            .and_then(|config| config.get("features.profiles"))
            .and_then(Value::as_str)
            == Some("true");
        if own { project } else { DEFAULT_PROJECT }.to_string()
    }

    /// The expanded devices and config of an instance body in `project`: each profile it names, in order, then its own keys.
    /// `None` when the body names no profiles. A profile that is not there adds nothing, as an empty `default` profile does.
    fn expansion(
        &self,
        project: &str,
        body: &Value,
    ) -> Option<(Map<String, Value>, Map<String, Value>)> {
        let profiles = body.get("profiles")?.as_array()?;
        let home = self.profile_home(project);
        let mut devices = Map::new();
        let mut config = Map::new();
        let layers = profiles
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|name| {
                self.objects
                    .iter()
                    .find(|object| {
                        object.kind == "profile" && object.project == home && object.name == name
                    })
                    .map(|object| &object.body)
            })
            .chain(std::iter::once(body));
        for layer in layers {
            for (key, into) in [("devices", &mut devices), ("config", &mut config)] {
                if let Some(Value::Object(map)) = layer.get(key) {
                    into.extend(map.clone());
                }
            }
        }
        Some((devices, config))
    }

    /// The devices and config an instance body has in `project`: its expansion, or its own keys when it names no profiles.
    fn effective(&self, project: &str, body: &Value) -> (Map<String, Value>, Map<String, Value>) {
        self.expansion(project, body).unwrap_or_else(|| {
            let own = |key: &str| {
                body.get(key)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default()
            };
            (own("devices"), own("config"))
        })
    }

    /// Works out the expanded devices and config of the instance at `index` again. With `reset`, the volatile keys of each
    /// device the new expansion drops or changes are cleared first (see [`reset_keys`]).
    fn expand(&mut self, index: usize, reset: bool) {
        let object = &self.objects[index];
        if object.kind != "instance" {
            return;
        }
        let Some((devices, _)) = self.expansion(&object.project, &object.body) else {
            return;
        };
        let cleared = if reset {
            let old = object
                .body
                .get("expanded_devices")
                .and_then(Value::as_object);
            let config = object.body.get("config").and_then(Value::as_object);
            reset_keys(
                old.cloned().unwrap_or_default(),
                &devices,
                config.cloned().unwrap_or_default(),
            )
        } else {
            Vec::new()
        };
        let project = object.project.clone();
        if let Some(config) = self.objects[index]
            .body
            .get_mut("config")
            .and_then(Value::as_object_mut)
        {
            for key in &cleared {
                config.remove(key);
            }
        }
        let Some((devices, config)) = self.expansion(&project, &self.objects[index].body) else {
            return;
        };
        if let Some(body) = self.objects[index].body.as_object_mut() {
            body.insert("expanded_devices".to_string(), Value::Object(devices));
            body.insert("expanded_config".to_string(), Value::Object(config));
        }
    }

    /// Gives `mac` the lease of `address` on `network`, in place of any lease it held.
    fn lease(&mut self, network: &str, mac: &str, address: &str, fixed: bool) {
        self.leases.retain(|lease| lease.hwaddr != mac);
        self.leases.push(Lease {
            network: network.to_string(),
            hwaddr: mac.to_string(),
            address: address.to_string(),
            fixed,
        });
    }

    /// The first host address from .100 in the network's own /24 that no lease holds.
    fn free_address(&self, network: &str) -> Option<String> {
        let own = self
            .objects
            .iter()
            .find(|object| object.kind == "network" && object.name == network)?
            .body
            .get("config")?
            .get("ipv4.address")?
            .as_str()?;
        let gateway: std::net::Ipv4Addr = own.split_once('/')?.0.parse().ok()?;
        let [a, b, c, _] = gateway.octets();
        (100..=250)
            .map(|d| std::net::Ipv4Addr::new(a, b, c, d))
            .filter(|candidate| *candidate != gateway)
            .map(|candidate| candidate.to_string())
            .find(|candidate| !self.leases.iter().any(|lease| lease.address == *candidate))
    }

    /// Starts the instance at `index`. Each NIC on a network gets its MAC (a new one is kept in `volatile.<dev>.hwaddr`) and
    /// its IPv4: a device's `ipv4.address` pins its lease, and a MAC without a lease is leased a free address.
    fn start(&mut self, index: usize, seed: u64) {
        for (name, device) in nics(&self.objects[index].body) {
            let Some(network) = device.get("network").and_then(Value::as_str) else {
                continue;
            };
            let mac = match nic_mac(&self.objects[index].body, &name, &device) {
                Some(mac) => mac,
                None => {
                    let mac = format!(
                        "10:66:6a:{:02x}:{:02x}:{:02x}",
                        (seed >> 16) & 0xff,
                        (seed >> 8) & 0xff,
                        seed & 0xff
                    );
                    if let Some(config) = self.objects[index]
                        .body
                        .get_mut("config")
                        .and_then(Value::as_object_mut)
                    {
                        config.insert(format!("volatile.{name}.hwaddr"), json!(mac));
                    }
                    mac
                }
            };
            match device.get("ipv4.address").and_then(Value::as_str) {
                Some(fixed) => self.lease(network, &mac, fixed, true),
                None if self.leases.iter().any(|lease| lease.hwaddr == mac) => {}
                None => {
                    if let Some(address) = self.free_address(network) {
                        self.lease(network, &mac, &address, false);
                    }
                }
            }
        }
        if let Some(body) = self.objects[index].body.as_object_mut() {
            body.insert("status".to_string(), json!("Running"));
        }
        self.expand(index, false);
    }

    /// Whether `project` sets `restricted` to `"true"`.
    fn restricted(&self, project: &str) -> bool {
        self.project_config(project)
            .and_then(|config| config.get("restricted"))
            .and_then(Value::as_str)
            == Some("true")
    }

    /// Whether a restricted project's key `key` is `"allow"`.
    fn allows(&self, project: &str, key: &str) -> bool {
        self.project_config(project)
            .and_then(|config| config.get(key))
            .and_then(Value::as_str)
            == Some("allow")
    }

    /// Why the instance body moved into `project` is refused, when it is, as Incus 7.5 checks a moved-in instance's devices
    /// and config in the project, expanded with that project's profiles. A NIC's ACL must exist in the project that serves
    /// its network: its own when `features.networks` is on, and `default` otherwise. A restricted project then checks its
    /// restrictions as `checkRestrictions` does (`internal/server/project/permissions.go`), each `restricted.*` key at the
    /// value it is set to, or at Incus's default. This model is the fake's own, written from that source apart from the
    /// adoption writer's table, so a test of the writer against it is not the table checked against itself.
    fn moved_in_refusal(&self, project: &str, body: &Value) -> Option<String> {
        let (devices, config) = self.effective(project, body);
        let device_field = |device: &Value, key: &str| {
            device
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let acl_home = if self
            .project_config(project)
            .and_then(|config| config.get("features.networks"))
            .and_then(Value::as_str)
            == Some("true")
        {
            project.to_string()
        } else {
            DEFAULT_PROJECT.to_string()
        };
        for (name, device) in &devices {
            if device_field(device, "type") != "nic" {
                continue;
            }
            let acls = device_field(device, "security.acls");
            for acl in acls.split(',').map(str::trim).filter(|acl| !acl.is_empty()) {
                let found = self.objects.iter().any(|object| {
                    object.kind == "acl" && object.project == acl_home && object.name == acl
                });
                if !found {
                    return Some(format!(
                        "Invalid device {name:?}: network ACL {acl:?} not found in project {acl_home:?}"
                    ));
                }
            }
        }
        if !self.restricted(project) {
            return None;
        }
        let restriction = |key: &str| -> String {
            self.project_config(project)
                .and_then(|config| config.get(key))
                .and_then(Value::as_str)
                .map_or_else(
                    || {
                        match key {
                            "restricted.containers.privilege" => "unprivileged",
                            "restricted.devices.nic" | "restricted.devices.disk" => "managed",
                            "restricted.devices.disk.paths" | "restricted.networks.access" => "",
                            _ => "block",
                        }
                        .to_string()
                    },
                    str::to_string,
                )
        };
        let forbidden = |what: &str| Some(format!("Project restricted: {what} are forbidden"));
        for (name, device) in &devices {
            let field = |key: &str| device_field(device, key);
            match field("type").as_str() {
                "nic" => {
                    let nic = restriction("restricted.devices.nic");
                    if nic == "block" || (nic == "managed" && field("network").is_empty()) {
                        return forbidden(&format!("the network device {name}"));
                    }
                    let grant = restriction("restricted.networks.access");
                    let network = field("network");
                    if !network.is_empty()
                        && !grant.is_empty()
                        && !grant
                            .split(',')
                            .map(str::trim)
                            .any(|listed| listed == network)
                    {
                        return Some(format!(
                            "Invalid device {name:?}: Network not allowed in project"
                        ));
                    }
                }
                "disk" if field("path") == "/" && !field("pool").is_empty() => {}
                "disk" if field("pool").is_empty() => {
                    let paths = restriction("restricted.devices.disk.paths");
                    let source = format!("{}/", field("source").trim_end_matches('/'));
                    let under = |parent: &str| {
                        source.starts_with(&format!("{}/", parent.trim_end_matches('/')))
                    };
                    let listed = paths.is_empty() || paths.split(',').any(under);
                    if restriction("restricted.devices.disk") != "allow" || !listed {
                        return forbidden(&format!("the host path disk {name}"));
                    }
                }
                "disk" if restriction("restricted.devices.disk") == "block" => {
                    return forbidden(&format!("the disk {name}"));
                }
                kind @ ("unix-char" | "unix-block" | "unix-hotplug" | "infiniband" | "gpu"
                | "usb" | "pci" | "proxy")
                    if restriction(&format!("restricted.devices.{kind}")) != "allow" =>
                {
                    return forbidden(&format!("{kind} devices"));
                }
                _ => {}
            }
        }
        let truthy = |value: &str| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            )
        };
        let falsy = |value: &str| {
            value.is_empty()
                || matches!(
                    value.to_ascii_lowercase().as_str(),
                    "false" | "0" | "no" | "off"
                )
        };
        let lowlevel = restriction("restricted.containers.lowlevel") == "allow";
        for (key, value) in &config {
            let value = value.as_str().unwrap_or("");
            if FAKE_INTERCEPT.contains(&key.as_str()) {
                if !falsy(value) && restriction("restricted.containers.interception") != "allow" {
                    return forbidden("container syscall interceptions");
                }
            } else if (key.starts_with("security.syscalls.intercept")
                || key.starts_with("security.bpffs")
                || key.starts_with("raw.")
                || FAKE_LOW_LEVEL.contains(&key.as_str()))
                && !lowlevel
            {
                return Some(format!("Use of low-level config {key:?} is forbidden"));
            }
            match key.as_str() {
                "security.privileged"
                    if truthy(value)
                        && restriction("restricted.containers.privilege") != "allow" =>
                {
                    return forbidden("privileged containers");
                }
                "security.nesting"
                    if truthy(value) && restriction("restricted.containers.nesting") == "block" =>
                {
                    return forbidden("nested containers");
                }
                _ => {}
            }
        }
        None
    }
}

/// The interception keys Incus 7.5 lets `restricted.containers.interception` allow (`allowableIntercept`).
const FAKE_INTERCEPT: [&str; 7] = [
    "security.syscalls.intercept.bpf",
    "security.syscalls.intercept.bpf.devices",
    "security.syscalls.intercept.mknod",
    "security.syscalls.intercept.mount",
    "security.syscalls.intercept.mount.fuse",
    "security.syscalls.intercept.setxattr",
    "security.syscalls.intercept.sysinfo",
];

/// The container keys Incus 7.5 counts as low level (`isContainerLowLevelOptionForbidden`), beside every
/// `security.syscalls.intercept*` key not in [`FAKE_INTERCEPT`], every `raw.*` key and every `security.bpffs*` key.
const FAKE_LOW_LEVEL: [&str; 12] = [
    "boot.host_shutdown_action",
    "boot.host_shutdown_timeout",
    "linux.kernel_modules",
    "limits.memory.swap",
    "limits.memory.oom_priority",
    "security.guestapi.images",
    "security.idmap.base",
    "security.idmap.size",
    "security.selinux.domain",
    "security.selinux.label_rootfs",
    "security.selinux.level",
    "security.selinux.type",
];

/// The volatile keys `deviceVolatileReset` clears when an instance's expanded devices go from `old` to `new`. A device that is
/// gone, or whose type changes, loses every `volatile.<dev>.*` key. A device that changes in more than its addresses loses each
/// `volatile.<dev>.<key>` whose `<key>` the new device sets. A device that does not change keeps them all.
fn reset_keys(
    old: Map<String, Value>,
    new: &Map<String, Value>,
    config: Map<String, Value>,
) -> Vec<String> {
    let strip = |device: &Value| -> Option<Map<String, Value>> {
        device.as_object().map(|map| {
            map.iter()
                .filter(|(key, _)| !UPDATABLE_NIC_KEYS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
    };
    let mut cleared = Vec::new();
    for (name, before) in &old {
        let after = new.get(name);
        if after.is_some_and(|after| strip(after) == strip(before)) {
            continue;
        }
        let prefix = format!("volatile.{name}.");
        let retyped = after.is_none_or(|after| after.get("type") != before.get("type"));
        for key in config.keys().filter(|key| key.starts_with(&prefix)) {
            let field = &key[prefix.len()..];
            if retyped || after.is_some_and(|after| after.get(field).is_some()) {
                cleared.push(key.clone());
            }
        }
    }
    cleared
}

/// The NIC devices an instance has: its expanded devices when it has them, and its own otherwise.
fn nics(body: &Value) -> Vec<(String, Map<String, Value>)> {
    body.get("expanded_devices")
        .or_else(|| body.get("devices"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, device)| {
            device
                .as_object()
                .filter(|device| device.get("type").and_then(Value::as_str) == Some("nic"))
                .map(|device| (name.clone(), device.clone()))
        })
        .collect()
}

/// A NIC's MAC: its device's `hwaddr`, else its `volatile.<dev>.hwaddr`.
fn nic_mac(body: &Value, name: &str, device: &Map<String, Value>) -> Option<String> {
    device
        .get("hwaddr")
        .and_then(Value::as_str)
        .filter(|mac| !mac.is_empty())
        .or_else(|| {
            body.get("config")?
                .get(format!("volatile.{name}.hwaddr"))?
                .as_str()
        })
        .map(str::to_string)
}

/// An instance's state: its status and, while it runs, each NIC's MAC and the IPv4 its device pins or its MAC leases.
fn instance_state(store: &Store, address: &Address) -> Answer {
    let Some(index) = store.find(address) else {
        return (404, String::new(), error_envelope(404, "not found"));
    };
    let body = &store.objects[index].body;
    let status = text_of(body, "status");
    let running = status == "Running";
    let mut network = Map::new();
    for (name, device) in nics(body).into_iter().filter(|_| running) {
        let mac = nic_mac(body, &name, &device).unwrap_or_default();
        let address = device
            .get("ipv4.address")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                store
                    .leases
                    .iter()
                    .find(|lease| lease.hwaddr == mac)
                    .map(|lease| lease.address.clone())
            });
        let addresses: Vec<Value> = address
            .into_iter()
            .map(|address| json!({ "family": "inet", "address": address, "netmask": "24", "scope": "global" }))
            .collect();
        network.insert(
            name,
            json!({ "hwaddr": mac, "addresses": addresses, "state": "up", "type": "broadcast" }),
        );
    }
    sync(
        json!({ "status": status, "status_code": if running { 103 } else { 102 }, "network": network }),
    )
}

/// Starts or stops an instance from a PUT of its state, as an operation. A stop of a hung instance fails inside its operation
/// and leaves it running, unless it is forced and only the graceful stop hangs.
fn change_state(
    shared: &Shared,
    store: &mut Store,
    request: &Request,
    address: &Address,
) -> Answer {
    let bad = |message: &str| (400, String::new(), error_envelope(400, message));
    let Some(index) = store.find(address) else {
        return (404, String::new(), error_envelope(404, "not found"));
    };
    let Ok(Value::Object(sent)) = serde_json::from_slice::<Value>(&request.body) else {
        return bad("the body is not a JSON object");
    };
    let force = sent.get("force").and_then(Value::as_bool) == Some(true);
    let running = text_of(&store.objects[index].body, "status") == "Running";
    match text_in(&sent, "action").as_str() {
        "start" if running => bad("The instance is already running"),
        "start" => {
            store.start(index, shared.counter.fetch_add(1, Ordering::SeqCst));
            answered(shared, store, "instance", "PUT", Ok(()))
        }
        "stop" if !running => bad("The instance is already stopped"),
        "stop" => {
            let hung = store.hung.get(&store.objects[index].name).copied();
            if hung.is_some_and(|forced_too| forced_too || !force) {
                let failure = Err((400, "Failed shutting down instance".to_string()));
                return answered(shared, store, "instance", "PUT", failure);
            }
            if let Some(body) = store.objects[index].body.as_object_mut() {
                body.insert("status".to_string(), json!("Stopped"));
            }
            answered(shared, store, "instance", "PUT", Ok(()))
        }
        _ => bad("the fake takes only a start or a stop"),
    }
}

/// Takes a snapshot of an instance, or a backup, which the fake does not keep. A restricted project refuses either unless its
/// `restricted.snapshots` or `restricted.backups` is `allow`.
fn snapshot_or_backup(
    shared: &Shared,
    store: &mut Store,
    request: &Request,
    address: &Address,
    kind: &str,
) -> Answer {
    let instance = Address {
        kind: "instance",
        project: address.project.clone(),
        parent: String::new(),
        name: address.parent.clone(),
    };
    if store.find(&instance).is_none() {
        return (404, String::new(), error_envelope(404, "not found"));
    }
    if store.restricted(&address.project)
        && !store.allows(&address.project, &format!("restricted.{kind}s"))
    {
        return (
            403,
            String::new(),
            error_envelope(403, &format!("Project restricted: {kind}s are forbidden")),
        );
    }
    let Ok(Value::Object(sent)) = serde_json::from_slice::<Value>(&request.body) else {
        return (
            400,
            String::new(),
            error_envelope(400, "the body is not a JSON object"),
        );
    };
    let mut name = text_in(&sent, "name");
    if kind == "backup" && name.is_empty() {
        // Incus names an unnamed backup after the count of the instance's backups.
        let count = store
            .objects
            .iter()
            .filter(|object| {
                object.kind == kind
                    && object.project == address.project
                    && object.parent == address.parent
            })
            .count();
        name = format!("backup{count}");
    }
    if name.is_empty() || name.contains('/') {
        return (
            400,
            String::new(),
            error_envelope(400, "the snapshot has no usable name"),
        );
    }
    let place = Address {
        name: name.clone(),
        ..address.clone()
    };
    if store.find(&place).is_some() {
        return (
            409,
            String::new(),
            error_envelope(409, "the snapshot already exists"),
        );
    }
    store.objects.push(Stored {
        kind: kind.to_string(),
        project: address.project.clone(),
        name: name.clone(),
        parent: address.parent.clone(),
        body: json!({ "name": name, "stateful": sent.get("stateful").cloned().unwrap_or(json!(false)) }),
    });
    answered(shared, store, kind, "POST", Ok(()))
}

/// Moves a stopped instance to the project its POST names, under its own name, with its snapshots, as an operation. As Incus
/// 7.5 does, the move is a copy: the instance gets a new `volatile.uuid` and `volatile.apply_template` is `copy`. Its devices
/// are expanded in the new project with the volatile reset. A rename, a migration or any other POST is refused.
fn move_instance(
    shared: &Shared,
    store: &mut Store,
    request: &Request,
    address: &Address,
) -> Answer {
    let bad = |code: u16, message: &str| (code, String::new(), error_envelope(code, message));
    let Some(index) = store.find(address) else {
        return bad(404, "not found");
    };
    let Ok(Value::Object(sent)) = serde_json::from_slice::<Value>(&request.body) else {
        return bad(400, "the body is not a JSON object");
    };
    let name = text_in(&sent, "name");
    let to = text_in(&sent, "project");
    let plain = sent.keys().all(|key| key == "name" || key == "project");
    if !plain || name != address.name || to.is_empty() || to == address.project {
        return bad(
            400,
            "the fake only moves an instance to another project under its own name",
        );
    }
    if text_of(&store.objects[index].body, "status") == "Running" {
        return bad(400, "Instance must be stopped to be moved");
    }
    if store.project_config(&to).is_none() {
        return bad(404, "Project not found");
    }
    let taken = Address {
        project: to.clone(),
        ..address.clone()
    };
    if store.find(&taken).is_some() {
        return bad(409, "the name is taken in the target project");
    }
    if let Some(reason) = store.moved_in_refusal(&to, &store.objects[index].body) {
        return bad(403, &reason);
    }
    let from = std::mem::replace(&mut store.objects[index].project, to.clone());
    if let Some(body) = store.objects[index].body.as_object_mut() {
        body.insert("project".to_string(), json!(to));
        if let Some(config) = body.get_mut("config").and_then(Value::as_object_mut) {
            config.insert("volatile.uuid".to_string(), json!(shared.next_id()));
            config.insert("volatile.apply_template".to_string(), json!("copy"));
        }
    }
    for object in &mut store.objects {
        if object.kind == "snapshot" && object.project == from && object.parent == name {
            object.project = to.clone();
        }
    }
    store.expand(index, true);
    answered(shared, store, "instance", "POST", Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_parameter_is_found_by_its_exact_key() {
        assert_eq!(query_param("project=svc&x=1", "project"), Some("svc"));
        assert_eq!(query_param("xproject=svc", "project"), None);
        assert_eq!(query_param("", "project"), None);
    }

    #[test]
    fn the_instance_listing_is_served_at_recursion_one_and_two_only() {
        assert!(matches!(
            parse("/1.0/instances?all-projects=true&recursion=1"),
            Some(Route::ListInstances)
        ));
        assert!(matches!(
            parse("/1.0/instances?all-projects=true&recursion=2"),
            Some(Route::ListInstancesWithState)
        ));
        assert_eq!(
            parse("/1.0/instances?all-projects=true&recursion=2").map(|r| r.methods()),
            Some(&["GET"][..])
        );
    }

    #[test]
    fn an_unknown_path_has_no_route_and_a_known_collection_takes_only_post() {
        assert!(parse("/1.0/nope").is_none());
        assert!(matches!(parse("/1.0/projects"), Some(Route::Collection(_))));
        assert!(matches!(
            parse("/1.0/projects?recursion=1"),
            Some(Route::ListProjects)
        ));
        assert_eq!(
            parse("/1.0/instances").map(|r| r.methods()),
            Some(&["POST"][..])
        );
        assert_eq!(
            parse("/1.0/instances/a").map(|r| r.methods()),
            Some(&["GET", "POST", "PUT", "PATCH", "DELETE"][..])
        );
        assert_eq!(
            parse("/1.0/instances/a/state").map(|r| r.methods()),
            Some(&["GET", "PUT"][..])
        );
    }

    #[test]
    fn a_merge_removes_a_key_set_to_empty_and_replaces_top_level_keys() {
        let mut object = json!({"config": {"a": "1", "b": "2"}, "description": "x"})
            .as_object()
            .cloned()
            .unwrap_or_default();
        let changes = json!({"config": {"a": "", "c": "3"}, "description": "y"})
            .as_object()
            .cloned()
            .unwrap_or_default();
        merge(&mut object, &changes).expect("a valid merge");
        assert_eq!(object["config"], json!({"b": "2", "c": "3"}));
        assert_eq!(object["description"], json!("y"));
    }

    #[test]
    fn a_config_value_that_is_not_text_is_refused_by_the_merge() {
        let mut object = Map::new();
        let changes = json!({"config": {"a": 1}})
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert!(merge(&mut object, &changes).is_err());
    }

    #[test]
    fn the_etag_changes_with_the_object_and_is_quoted() {
        let first = etag_of(&json!({"config": {"a": "1"}}));
        let second = etag_of(&json!({"config": {"a": "2"}}));
        assert_ne!(first, second);
        assert!(first.starts_with('"') && first.ends_with('"'));
    }
}

/// Hermetic scripts for the closed service seam. Unscripted commands exit 127.
#[derive(Default)]
struct ServiceFake {
    scripts: std::collections::VecDeque<ExecScript>,
    calls: Vec<ServiceRequest>,
    recordings: BTreeMap<String, Vec<u8>>,
    files: BTreeMap<String, Vec<u8>>,
}

/// Request evidence for tests only; bodies may contain test secrets, so this has no Debug implementation.
#[derive(Clone)]
pub struct ServiceRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct ExecScript {
    pub argv: Vec<String>,
    pub exit: i64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub held: bool,
    pub swap_uuid: bool,
    pub post_status: u16,
}
impl ExecScript {
    pub fn new(argv: Vec<String>, exit: i64, stdout: &str) -> Self {
        Self {
            argv,
            exit,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            held: false,
            swap_uuid: false,
            post_status: 200,
        }
    }
}
impl FakeIncus {
    pub fn script_exec(&self, script: ExecScript) {
        lock(&self.shared.service).scripts.push_back(script);
    }
    pub fn service_requests(&self) -> Vec<ServiceRequest> {
        lock(&self.shared.service).calls.clone()
    }
    pub fn recordings(&self) -> usize {
        lock(&self.shared.service).recordings.len()
    }
    pub fn service_files(&self) -> BTreeMap<String, Vec<u8>> {
        lock(&self.shared.service).files.clone()
    }
}

fn service_response(shared: &Shared, request: &Request) -> Option<(u16, Vec<u8>)> {
    let segments: Vec<&str> = request.path().split('/').collect();
    let ["", "1.0", "instances", name, rest @ ..] = segments.as_slice() else {
        return None;
    };
    if !matches!(rest, ["exec"] | ["files"] | ["logs", "exec-output", _]) {
        return None;
    }
    let query = request.target.split_once('?').map_or("", |(_, q)| q);
    let project = query_param(query, "project").unwrap_or(DEFAULT_PROJECT);
    let mut service = lock(&shared.service);
    service.calls.push(ServiceRequest {
        method: request.method.clone(),
        path: request.target.clone(),
        headers: request.headers.clone(),
        body: request.body.clone(),
    });
    let encoded = |code, message| {
        (
            code,
            serde_json::to_vec(&error_envelope(code, message)).unwrap(),
        )
    };
    let mut store = lock(&shared.store);
    let Some(index) = store
        .objects
        .iter()
        .position(|o| o.kind == "instance" && o.project == project && o.name == *name)
    else {
        return Some(encoded(404, "not found"));
    };
    if let ["logs", "exec-output", _] = rest {
        let key = format!("{project}:{}", request.path());
        return Some(match request.method.as_str() {
            "GET" => service
                .recordings
                .get(&key)
                .cloned()
                .map_or_else(|| encoded(404, "not found"), |bytes| (200, bytes)),
            "DELETE" => {
                if service.recordings.remove(&key).is_some() {
                    (200, b"{}".to_vec())
                } else {
                    encoded(404, "not found")
                }
            }
            _ => encoded(405, "method"),
        });
    }
    if rest == ["files"] {
        let path = crate::read::percent_decode(query_param(query, "path").unwrap_or(""));
        let key = format!("{project}/{name}:{path}");
        return Some(match request.method.as_str() {
            "POST" => {
                service.files.insert(key, request.body.clone());
                (200, b"{}".to_vec())
            }
            "GET" => service
                .files
                .get(&key)
                .cloned()
                .map_or_else(|| encoded(404, "not found"), |bytes| (200, bytes)),
            "DELETE" => {
                service.files.remove(&key);
                (200, b"{}".to_vec())
            }
            _ => encoded(405, "method"),
        });
    }
    if request.method != "POST" {
        return Some(encoded(405, "method"));
    }
    if store.objects[index].body["status"] != "Running" {
        return Some(encoded(400, "instance is stopped"));
    }
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let argv: Vec<String> = body["command"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let script = if service.scripts.front().is_some_and(|s| s.argv == argv) {
        service.scripts.pop_front().unwrap()
    } else {
        ExecScript::new(argv, 127, "")
    };
    if script.post_status != 200 {
        return Some(encoded(script.post_status, "scripted failure"));
    }
    let id = shared.next_id();
    let mut op = running_operation(&id, &place_of_stored(&store.objects[index]));
    op["description"] = json!("Executing command");
    op["metadata"] = json!({"return":script.exit});
    if !script.held {
        op["status"] = json!("Success");
        op["status_code"] = json!(200);
    } else {
        store.held.insert(id.clone(), Held::default());
    }
    if script.swap_uuid {
        store.objects[index].body["config"]["volatile.uuid"] = json!(shared.next_id());
    }
    if body["command"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v == "/usr/bin/rm"))
    {
        service
            .files
            .retain(|key, _| !key.starts_with(&format!("{project}/{name}:/run/arca-ts/")));
    }
    for (extension, bytes) in [("stdout", script.stdout), ("stderr", script.stderr)] {
        service.recordings.insert(
            format!("{project}:/1.0/instances/{name}/logs/exec-output/exec_{id}.{extension}"),
            bytes,
        );
    }
    store.operations.insert(id, op.clone());
    Some((200, serde_json::to_vec(&op).unwrap()))
}
